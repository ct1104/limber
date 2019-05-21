//! Export command module for Limber.
//!
//! This module exposes functions to export an Elasticsearch target index to
//! `stdio`. This allows the caller to pipe into any compression algorithms
//! they may wish to use, and store in any container they might wish to use.
//!
//! This interface also allows chaining into another instance of Limber, to
//! enable piping from one cluster/index to another in a streaming fashion.
use clap::{value_t, ArgMatches};
use elastic::client::requests::{ScrollRequest, SearchRequest};
use elastic::client::AsyncClientBuilder;
use elastic::prelude::*;
use failure::{format_err, Error};
use futures::future::{self, Either, Loop};
use futures::prelude::*;
use serde_json::{json, Value};
use url::Url;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Constructs a `Future` to execute the `export` command.
///
/// This future should be spawned on a Runtime to carry out the exporting
/// process. The returned future will be a combination of several futures
/// to represent the concurrency flags provided via the CLI arguments.
pub fn run(args: &ArgMatches) -> Box<Future<Item = (), Error = Error>> {
    // fetch the number of workers to use to export, default to CPU counts
    let workers = value_t!(args, "workers", usize).unwrap_or_else(|_| num_cpus::get());

    // parse arguments into a host/index pairing for later
    let (host, index) = match parse_cluster_info(&args) {
        Ok(info) => info,
        Err(e) => {
            let err = future::err(e);
            return Box::new(err);
        }
    };

    // construct a single client instance to be used across all tasks
    let client = match AsyncClientBuilder::new().static_node(host).build() {
        Ok(client) => Arc::new(client),
        Err(e) => {
            let fmt = format_err!("{}", e.to_string());
            let err = future::err(fmt);
            return Box::new(err);
        }
    };

    // create counter to track documents added
    let counter = Arc::new(AtomicUsize::new(0));

    // create vec to store worker task futures
    let mut tasks = Vec::with_capacity(workers);

    // construct worker task
    for idx in 0..workers {
        // take ownership of stuff
        let index = index.clone();
        let client = client.clone();
        let counter = counter.clone();

        // create our initial search request to trigger scrolling
        let request = match construct_query(&args, idx, workers) {
            Ok(query) => SearchRequest::for_index(index, query),
            Err(e) => {
                let err = future::err(e);
                return Box::new(err);
            }
        };

        let execute = client
            .request(request)
            .params_fluent(|p| p.url_param("scroll", "1m"))
            .send()
            .and_then(AsyncResponseBuilder::into_response)
            .and_then(|value: Value| {
                future::loop_fn((counter, client, value), |(counter, client, mut value)| {
                    // fetch the hits back
                    let hits = value
                        .pointer_mut("/hits/hits")
                        .expect("unable to locate hits")
                        .as_array_mut()
                        .expect("hits are of wrong type");

                    // empty hits means we're done
                    if hits.is_empty() {
                        let ctx = (counter, client, value);
                        let brk = Loop::Break(ctx);
                        let okr = future::ok(brk);
                        return Either::B(okr);
                    }

                    // store hit length
                    let len = hits.len();

                    // iterate docs
                    for hit in hits {
                        // grab a mutable reference to the document
                        let container = hit.as_object_mut().unwrap();

                        // drop some query based fields
                        container.remove("sort");
                        container.remove("_score");

                        // drop it to stdout
                        println!("{}", hit);
                    }

                    // increment the counter and print the state to stderr
                    let cnt = counter.fetch_add(len, Ordering::Relaxed);
                    eprintln!("Fetched batch of {}, have now processed {}", len, cnt + len);

                    // fetch the new scroll_id
                    let scroll_id = value
                        .get("_scroll_id")
                        .expect("unable to locate scroll_id")
                        .as_str()
                        .expect("scroll_id is of wrong type")
                        .to_owned();

                    // construct the request for the next batch
                    let request = ScrollRequest::for_scroll_id(
                        scroll_id,
                        json!({
                            "scroll": "1m"
                        }),
                    );

                    // loop on the next batch
                    Either::A(
                        client
                            .request(request)
                            .send()
                            .and_then(AsyncResponseBuilder::into_response)
                            .and_then(|value: Value| Ok(Loop::Continue((counter, client, value)))),
                    )
                })
            });

        // push the worker
        tasks.push(execute);
    }

    // join all tasks
    Box::new(
        future::join_all(tasks)
            .map_err(|e| format_err!("{}", e.to_string()))
            .map(|_| ()),
    )
}

/// Attempts to parse a host/index pair out of the CLI arguments.
///
/// This logic is pretty vague; we don't actually test connection beyond
/// looking to see if the provided scheme is HTTP(S). The index string
/// returned will never be empty; if no index is provided, we'll use the
/// ES "_all" alias to avoid having to deal with `Option` types for now.
fn parse_cluster_info(args: &ArgMatches) -> Result<(String, String), Error> {
    // fetch the source from the arguments, should always be possible
    let source = args.value_of("source").expect("guaranteed by CLI");

    // attempt to parse the resource
    let mut url = Url::parse(source)?;

    // this is invalid, so not entirely sure what to do here
    if !url.has_host() || !url.scheme().starts_with("http") {
        return Err(format_err!("Invalid cluster resource provided"));
    }

    // fetch index from path, trimming the prefix
    let index = url.path().trim_start_matches('/');

    // set default index
    if index.is_empty() {
        "_all"
    } else {
        index
    };

    // take ownership to enable mut url
    let index = index.to_owned();

    // trim the path
    url.set_path("");

    // assume we have a cluster now, so pass it back
    Ok((url.as_str().trim_end_matches('/').to_owned(), index))
}

/// Constructs a query instance based on the worker count and identifier.
///
/// This can technically fail if the query provided is invalid, which is why
/// the return type is a `Result`. This is the safest option, as the user will
/// expect their results to be correctly filtered.
///
/// An error in this case should halt all progress by the main export process.
fn construct_query(args: &ArgMatches, id: usize, max: usize) -> Result<Value, Error> {
    // fetch the configured batch size, or default to 100
    let size = value_t!(args, "size", usize).unwrap_or(100);

    // fetch the query filter to use to limit matches (defaults to all docs)
    let filter = args.value_of("query").unwrap_or("{\"match_all\":{}}");
    let filter = serde_json::from_str::<Value>(filter)?;

    // construct query
    let mut query = json!({
        "query": filter,
        "size": size,
        "sort": [
            "_doc"
        ]
    });

    // handle multiple workers...
    if max > 1 {
        // ... by adding the slice identifier
        query.as_object_mut().unwrap().insert(
            "slice".to_owned(),
            json!({
                "id": id,
                "max": max
            }),
        );
    }

    // pass back!
    Ok(query)
}

/// Elastic search output with incremental backoff.
///
/// The algoritm is as follows:
///

/// Inputs:
///   * batch_size - number of messages in each batch.
///   * timeout - timeout for a write before we back off.
///   * concurrency - number of paralell batches.
///   * backoffs - array of backoffs to match.
///
/// Variables:
///   * backoff - additional delay after timed out send.
///
/// Pseudo variables:
///   * batch - collection of messages.
///   * queue - a queue of fugure sends.
///
/// Pseudocode:
/// ```
/// for m in messages {
///   if now() - last_done > backoff {
///     batch.add(m)
///     if batch.size >= batch_size {
///       if queue.size < concurrency {
///         queue.push(batch.send())
///       } else {
///         future = queue.first
///         if future.is_done {
///           queue.pop()
///           if future.execution_time < timeout {
///             backoff = 0;
///           } else {
///             backoff = grow_backoff(backoff) // backoff increase logic
///           }
///           last_done = now();
///           queue.push(batch.send())
///         } else {
///           batch.drop();
///         }
///       }
///     }
///   }
/// }
/// ```
use elastic::client::prelude::BulkErrorsResponse;
use elastic::client::requests::BulkRequest;
use elastic::client::{Client, SyncSender};
use elastic::prelude::SyncClientBuilder;
use error::TSError;
use output::{OUTPUT_DELIVERED, OUTPUT_DROPPED, OUTPUT_SKIPED};
use pipeline::{Event, Step};
use prometheus::{Gauge, HistogramVec};
use serde_json::{self, Value};
//use std::collections::HashMap;
use chrono::prelude::*;
use std::collections::VecDeque;
use std::convert::From;
use std::f64;
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;
use std::time::Instant;
use std::time::{SystemTime, UNIX_EPOCH};
use threadpool::ThreadPool;

lazy_static! {
    // Histogram of the duration it takes between getting a message and
    // sending (or dropping) it.
    static ref SEND_HISTOGRAM: HistogramVec = register_histogram_vec!(
        "ts_es_latency",
        "Latency for logstash output.",
        &["dest"],
        vec![
            0.0005, 0.001, 0.0025,
             0.005, 0.01, 0.025,
             0.05, 0.1, 0.25,
             0.5, 1.0, 2.5,
             5.0, 10.0, 25.0,
             50.0, 100.0, 250.0,
             500.0, 1000.0, 2500.0,
             f64::INFINITY]
    ).unwrap();
    static ref BACKOFF_GAUGE: Gauge = register_gauge!(opts!(
        "ts_es_backoff_ms",
        "Current backoff in millis."
    )).unwrap();

}

fn default_threads() -> usize {
    5
}

fn default_concurrency() -> usize {
    5
}

fn default_backoff() -> Vec<u64> {
    vec![50, 100, 250, 500, 1000, 5000, 10000]
}

fn default_append_date() -> bool {
    false
}

//[endpoints, index, batch_size, batch_timeout]
#[derive(Deserialize, Debug)]
struct Config {
    endpoints: Vec<String>,
    index: String,
    batch_size: usize,
    batch_timeout: f64,
    #[serde(default = "default_backoff")]
    backoff_rules: Vec<u64>,
    #[serde(default = "default_threads")]
    threads: usize,
    #[serde(default = "default_concurrency")]
    concurrency: usize,
    prefix_key: Option<String>,
    #[serde(default = "default_append_date")]
    append_date: bool,
}

impl Config {
    pub fn next_backoff(&self, last_backoff: u64) -> u64 {
        for backoff in &self.backoff_rules {
            if *backoff > last_backoff {
                return *backoff;
            }
        }
        last_backoff
    }
}

struct AsyncSink<T> {
    queue: VecDeque<Receiver<Result<T, TSError>>>,
    capacity: usize,
    size: usize,
}

enum SinkEnqueueError {
    AtCapacity,
}

enum SinkDequeueError {
    Empty,
    NotReady,
}

/// A queue of async tasks defined by an receiver that returns once the task
/// completes.
impl<T> AsyncSink<T> {
    pub fn new(capacity: usize) -> Self {
        AsyncSink {
            queue: VecDeque::with_capacity(capacity),
            capacity,
            size: 0,
        }
    }
    pub fn enqueue(&mut self, value: Receiver<Result<T, TSError>>) -> Result<(), SinkEnqueueError> {
        if self.size >= self.capacity {
            println!("size({}) >= capacity({})", self.size, self.capacity);
            Err(SinkEnqueueError::AtCapacity)
        } else {
            self.size += 1;
            self.queue.push_back(value);
            Ok(())
        }
    }
    pub fn dequeue(&mut self) -> Result<Result<T, TSError>, SinkDequeueError> {
        match self.queue.pop_front() {
            None => Err(SinkDequeueError::Empty),
            Some(rx) => match rx.try_recv() {
                Err(_) => {
                    self.queue.push_front(rx);
                    Err(SinkDequeueError::NotReady)
                }
                Ok(result) => {
                    self.size -= 1;
                    Ok(result)
                }
            },
        }
    }
    pub fn has_capacity(&self) -> bool {
        self.size < self.capacity
    }
}

impl From<SinkEnqueueError> for TSError {
    fn from(e: SinkEnqueueError) -> TSError {
        match e {
            SinkEnqueueError::AtCapacity => TSError::new("Queue overflow"),
        }
    }
}

#[derive(Clone)]
struct Destination {
    client: Client<SyncSender>,
    url: String,
}

pub struct Output {
    client_idx: usize,
    clients: Vec<Destination>,
    backoff: u64,
    queue: AsyncSink<f64>,
    qidx: usize,
    payload: String,
    last_flush: Instant,
    pool: ThreadPool,
    config: Config,
}

impl Output {
    /// Creates a new output connector, `brokers` is a coma seperated list of
    /// brokers to connect to. `topic` is the topic to send to.
    pub fn new(opts: &str) -> Self {
        match serde_json::from_str(opts) {
            Ok(config @ Config{..}) => {
                let clients = config.endpoints.iter().map(|client| Destination{
                    client: SyncClientBuilder::new().base_url(client.clone()).build().unwrap(),
                    url: client.clone()
                }).collect();
                let pool = ThreadPool::new(config.threads);
                let queue = AsyncSink::new(config.concurrency);
                Output {
                    client_idx: 0,
                    config,
                    backoff: 0,
                    pool,
                    clients,
                    qidx: 0,
                    payload: String::new(),
                    last_flush: Instant::now(),
                    queue
                }
            }
            _ => panic!("Invalid options for Elastic output, use `{{\"endpoints\":[\"<url>\"[, ...]], \"index\":\"<index>\", \"batch_size\":<size of each batch>, \"batch_timeout\": <maximum allowed timeout per batch>,[ \"threads\": <number of threads used to serve asyncornous writes>, \"concurrency\": <maximum number of batches in flight at any time>, \"backoff_rules\": [<1st timeout in ms>, <second timeout in ms>, ...], \"prefix_key\": \"<key to use as prefix>\", \"append_date\": <bool>]}}`"),
        }
    }

    fn send_future(&mut self) -> Receiver<Result<f64, TSError>> {
        self.client_idx = (self.client_idx + 1) % self.clients.len();
        let payload = self.payload.clone();
        let destination = self.clients[self.client_idx].clone();
        let c = self.qidx;
        let (tx, rx) = channel();
        self.pool.execute(move || {
            let dst = destination.url.as_str();
            let r = flush(&destination.client, dst, payload.as_str());
            match r.clone() {
                Ok(_) => OUTPUT_DELIVERED.with_label_values(&[dst]).inc_by(c as i64),
                Err(e) => {
                    println!("Error: {:?}", e);
                    OUTPUT_DROPPED.with_label_values(&[dst]).inc_by(c as i64);
                }
            };
            let _ = tx.send(r);
        });
        rx
    }
    fn inc_backoff(&mut self) {
        self.backoff = self.config.next_backoff(self.backoff);
        BACKOFF_GAUGE.set(self.backoff as f64);
    }

    fn reset_backoff(&mut self) {
        self.backoff = 0;
        BACKOFF_GAUGE.set(self.backoff as f64);
    }
    fn index(&self, event: &Event) -> String {
        let mut index = match self.config.prefix_key {
            None => self.config.index.clone(),
            Some(ref pfx) => match event.parsed {
                Value::Object(ref m) => match m.get(pfx) {
                    Some(Value::String(v)) => {
                        let mut index = v.clone();
                        index.push('_');
                        index.push_str(self.config.index.as_str());
                        index
                    }
                    _ => self.config.index.clone(),
                },
                _ => self.config.index.clone(),
            },
        };
        if self.config.append_date {
            let utc: DateTime<Utc> = Utc::now();
            index.push('-');
            index.push_str(utc.format("%Y.%m.%d").to_string().as_str());
            index
        } else {
            index
        }
    }
    fn doc_type(&self, event: &Event) -> String {
        match event.parsed {
            Value::Object(ref m) => match m.get("type") {
                Some(Value::String(v)) => v.clone(),
                _ => String::from("_doc"),
            },
            _ => String::from("_doc"),
        }
    }
}

fn flush(client: &Client<SyncSender>, url: &str, payload: &str) -> Result<f64, TSError> {
    let start = Instant::now();
    let timer = SEND_HISTOGRAM.with_label_values(&[url]).start_timer();
    let req = BulkRequest::new(payload.to_owned());
    client
        .request(req)
        .send()?
        .into_response::<BulkErrorsResponse>()?;
    timer.observe_duration();
    let d = start.elapsed();
    let d = duration_to_millis(d) as f64;
    Ok(d)
}

fn duration_to_millis(at: Duration) -> u64 {
    (at.as_secs() as u64 * 1_000) + (u64::from(at.subsec_nanos()) / 1_000_000)
}

fn update_send_time(event: Event) -> Result<String, serde_json::Error> {
    match event.parsed {
        Value::Object(mut m) => {
            let start = SystemTime::now();
            let since_the_epoch = start
                .duration_since(UNIX_EPOCH)
                .expect("Time went backwards");
            let tremor_map: serde_json::Map<String, Value> = [
                (
                    String::from("send_time"),
                    Value::Number(serde_json::Number::from(duration_to_millis(
                        since_the_epoch,
                    ))),
                ),
                (
                    String::from("classification"),
                    Value::String(event.classification),
                ),
            ].iter()
                .cloned()
                .collect();
            m.insert(String::from("_tremor"), Value::Object(tremor_map));
            serde_json::to_string(&Value::Object(m))
        }
        _ => serde_json::to_string(&event.parsed),
    }
}

impl Step for Output {
    fn apply(&mut self, event: Event) -> Result<Event, TSError> {
        let d = duration_to_millis(self.last_flush.elapsed());
        // We only add the message if it is not already dropped and
        // we are not in backoff time.
        if event.drop {
            OUTPUT_SKIPED.with_label_values(&[""]).inc();
            Ok(event)
        } else if d <= self.backoff {
            OUTPUT_DROPPED.with_label_values(&[""]).inc();
            Ok(event)
        } else {
            let out_event = event.clone();
            let index = self.index(&event);
            let doc_type = self.doc_type(&event);
            self.payload.push_str(
                json!({
                    "index":
                    {
                        "_index": index,
                        "_type": doc_type
                    }}).to_string()
                    .as_str(),
            );
            self.payload.push('\n');
            self.payload
                .push_str(update_send_time(event).unwrap().as_str());
            self.payload.push('\n');
            self.qidx += 1;

            if self.config.batch_size > self.qidx {
                Ok(out_event)
            } else {
                let r = match self.queue.dequeue() {
                    Err(SinkDequeueError::NotReady) => {
                        if self.queue.has_capacity() {
                            let rx = self.send_future();
                            self.queue.enqueue(rx)?;
                        } else {
                            OUTPUT_DROPPED
                                .with_label_values(&[""])
                                .inc_by(self.qidx as i64);
                        };
                        Ok(out_event)
                    }
                    Err(SinkDequeueError::Empty) => {
                        let rx = self.send_future();
                        self.queue.enqueue(rx)?;
                        Ok(out_event)
                    }
                    Ok(result) => {
                        let rx = self.send_future();
                        self.queue.enqueue(rx)?;
                        match result {
                            Ok(rtt) if rtt > self.config.batch_timeout as f64 => {
                                self.inc_backoff();
                                let mut event = Event::from(out_event);
                                event.feedback = Some(rtt);
                                Ok(event)
                            }
                            Err(e) => {
                                self.inc_backoff();
                                Err(e)
                            }
                            Ok(rtt) => {
                                self.reset_backoff();
                                let mut event = Event::from(out_event);
                                event.feedback = Some(rtt);
                                Ok(event)
                            }
                        }
                    }
                };
                self.payload.clear();
                self.qidx = 0;
                r
            }
        }
    }
}

// We don't do this in a test module since we need to access private functions.
#[test]
fn backoff_test() {
    let c = Config {
        endpoints: vec![String::from("")],
        index: String::from(""),
        batch_size: 10,
        batch_timeout: 10.0,
        backoff_rules: vec![10, 20, 30, 40],
        threads: 5,
        concurrency: 5,
        append_date: false,
        prefix_key: None,
    };
    assert_eq!(c.next_backoff(0), 10);
    assert_eq!(c.next_backoff(5), 10);
    assert_eq!(c.next_backoff(10), 20);
}

#[test]
fn index_test() {
    let s = Event::new("{\"key\":\"value\"}");
    let mut p = ::parser::new("json", "");
    let o = Output::new("{\"endpoints\":[\"http://elastic:9200\"], \"index\":\"demo\",\"batch_size\":100,\"batch_timeout\":500}");

    let r = p.apply(s).expect("couldn't parse data");
    let idx = o.index(&r);
    assert_eq!(idx, "demo");
}

#[test]
fn index_prefix_test() {
    let s = Event::new("{\"key\":\"value\"}");
    let mut p = ::parser::new("json", "");
    let o = Output::new("{\"endpoints\":[\"http://elastic:9200\"], \"index\":\"demo\",\"batch_size\":100,\"batch_timeout\":500, \"prefix_key\":\"key\"}");

    let r = p.apply(s).expect("couldn't parse data");
    let idx = o.index(&r);
    assert_eq!(idx, "value_demo");
}

#[test]
fn index_suffix_test() {
    println!("This test could be a false positive if it ran exactly at midnight, but that's OK.");
    let s = Event::new("{\"key\":\"value\"}");
    let mut p = ::parser::new("json", "");
    let o = Output::new("{\"endpoints\":[\"http://elastic:9200\"], \"index\":\"demo\",\"batch_size\":100,\"batch_timeout\":500, \"append_date\": true}");

    let r = p.apply(s).expect("couldn't parse data");
    let idx = o.index(&r);
    let utc: DateTime<Utc> = Utc::now();
    assert_eq!(
        idx,
        format!("demo-{}", utc.format("%Y.%m.%d").to_string().as_str())
    );
}

#[test]
fn index_prefix_suffix_test() {
    println!("This test could be a false positive if it ran exactly at midnight, but that's OK.");
    let s = Event::new("{\"key\":\"value\"}");
    let mut p = ::parser::new("json", "");
    let o = Output::new("{\"endpoints\":[\"http://elastic:9200\"], \"index\":\"demo\",\"batch_size\":100,\"batch_timeout\":500, \"append_date\": true, \"prefix_key\":\"key\"}");

    let r = p.apply(s).expect("couldn't parse data");
    let idx = o.index(&r);
    let utc: DateTime<Utc> = Utc::now();
    assert_eq!(
        idx,
        format!("value_demo-{}", utc.format("%Y.%m.%d").to_string().as_str())
    );
}
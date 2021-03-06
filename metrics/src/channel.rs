//  Copyright 2019 Twitter, Inc
//
//  Licensed under the Apache License, Version 2.0 (the "License");
//  you may not use this file except in compliance with the License.
//  You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
//  Unless required by applicable law or agreed to in writing, software
//  distributed under the License is distributed on an "AS IS" BASIS,
//  WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
//  See the License for the specific language governing permissions and
//  limitations under the License.

use crate::*;

use datastructures::*;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

#[derive(Debug)]
pub enum Measurement<C> {
    // taken from a counter eg: number of requests
    Counter { value: u64, time: u64 },
    // taken from a distribution eg: an external histogram
    Distribution { value: u64, count: C, time: u64 },
    // taken from a gauge eg: bytes of memory used
    Gauge { value: u64, time: u64 },
    // incremental count to sum into a counter
    Increment { count: C, time: u64 },
    // the start and stop of an event
    TimeInterval { start: u64, stop: u64 },
}

#[derive(PartialEq, Debug, Copy, Clone)]
pub enum Source {
    Counter,
    Distribution,
    Gauge,
    TimeInterval,
}

// #[derive(Clone)]
pub struct Channel<T>
where
    T: Counter + Unsigned,
    <T as AtomicPrimitive>::Primitive: Default + PartialEq + Copy + Saturating + From<u8>,
{
    name: Arc<Mutex<String>>,
    source: Source,
    counter: AtomicU64,
    histogram: Option<Histogram<T>>,
    last_write: AtomicU64,
    latched: bool,
    max: Point,
    min: Point,
    outputs: Arc<Mutex<HashSet<Output>>>,
    has_data: AtomicBool,
}

impl<T: 'static> PartialEq for Channel<T>
where
    T: Counter + Unsigned,
    <T as AtomicPrimitive>::Primitive: Default + PartialEq + Copy + Saturating + From<u8>,
    u64: From<<T as AtomicPrimitive>::Primitive>,
{
    fn eq(&self, other: &Channel<T>) -> bool {
        self.name() == other.name()
    }
}

impl<T: 'static> Eq for Channel<T>
where
    T: Counter + Unsigned,
    <T as AtomicPrimitive>::Primitive: Default + PartialEq + Copy + Saturating + From<u8>,
    u64: From<<T as AtomicPrimitive>::Primitive>,
{
}

impl<T: 'static> Channel<T>
where
    T: Counter + Unsigned,
    <T as AtomicPrimitive>::Primitive: Default + PartialEq + Copy + Saturating + From<u8>,
    u64: From<<T as AtomicPrimitive>::Primitive>,
{
    pub fn new(name: String, source: Source, histogram: Option<Histogram<T>>) -> Self {
        Self {
            name: Arc::new(Mutex::new(name)),
            source,
            counter: AtomicU64::default(),
            histogram,
            last_write: AtomicU64::default(),
            latched: true,
            max: Point::new(0, 0),
            min: Point::new(0, 0),
            outputs: Arc::new(Mutex::new(HashSet::new())),
            has_data: AtomicBool::new(false),
        }
    }

    pub fn name(&self) -> String {
        self.name.lock().unwrap().clone()
    }

    pub fn source(&self) -> Source {
        self.source
    }

    pub fn record(&self, measurement: Measurement<<T as AtomicPrimitive>::Primitive>) {
        match measurement {
            Measurement::Counter { value, time } => {
                self.record_counter(value, time);
            }
            Measurement::Distribution { value, count, time } => {
                self.record_distribution(value, count, time);
            }
            Measurement::Gauge { value, time } => {
                self.record_gauge(value, time);
            }
            Measurement::Increment { count, time } => self.record_increment(count, time),
            Measurement::TimeInterval { start, stop } => self.record_time_interval(start, stop),
        }
    }

    // for Counter measurements:
    // counter tracks value
    // histogram tracks rate of change
    fn record_counter(&self, value: u64, time: u64) {
        if self.source == Source::Counter {
            if self.has_data.load(Ordering::SeqCst) {
                // calculate the difference between consecutive readings and the rate
                let delta_value = value.wrapping_sub(self.counter.get());
                let delta_time = time.wrapping_sub(self.last_write.get());
                let rate = (delta_value as f64 * (1_000_000_000.0 / delta_time as f64)) as u64;
                self.counter.add(delta_value);
                if let Some(ref histogram) = self.histogram {
                    histogram.increment(rate, <T as AtomicPrimitive>::Primitive::from(1_u8));
                }
                // track the point of max rate
                if self.max.time() > 0 {
                    if rate > self.max.value() {
                        self.max.set(rate, time);
                    }
                } else {
                    self.max.set(rate, time);
                }
                // track the point of min rate
                if self.min.time() > 0 {
                    if rate < self.min.value() {
                        self.min.set(rate, time);
                    }
                } else {
                    self.min.set(rate, time);
                }
            } else {
                self.counter.set(value);
                self.has_data.store(true, Ordering::SeqCst);
            }
            self.last_write.set(time);
        }
    }

    // for Distribution measurements:
    // counter tracks sum of all counts
    // histogram tracks values
    fn record_distribution(&self, value: u64, count: <T as AtomicPrimitive>::Primitive, time: u64) {
        if self.source == Source::Distribution {
            self.counter.add(u64::from(count));
            if let Some(ref histogram) = self.histogram {
                histogram.increment(value, count);
            }
            self.last_write.set(time);
        }
    }

    // for Gauge measurements:
    // counter tracks latest reading
    // histogram tracks readings
    // max tracks largest reading
    // min tracks smallest reading
    fn record_gauge(&self, value: u64, time: u64) {
        if self.source == Source::Gauge {
            self.counter.set(value);
            if let Some(ref histogram) = self.histogram {
                histogram.increment(value, <T as AtomicPrimitive>::Primitive::from(1_u8));
            }
            // track the point of max gauge reading
            if self.max.time() > 0 {
                if value > self.max.value() {
                    self.max.set(value, time);
                }
            } else {
                self.max.set(value, time);
            }
            // track the point of min rate
            if self.min.time() > 0 {
                if value < self.min.value() {
                    self.min.set(value, time);
                }
            } else {
                self.min.set(value, time);
            }
            self.last_write.set(time);
        }
    }

    // for Increment measurements:
    // counter tracks sum of all increments
    // histogram tracks magnitude of increments
    fn record_increment(&self, count: <T as AtomicPrimitive>::Primitive, time: u64) {
        if self.source == Source::Counter {
            self.counter.add(u64::from(count));
            if let Some(ref histogram) = self.histogram {
                histogram.increment(
                    u64::from(count),
                    <T as AtomicPrimitive>::Primitive::from(1_u8),
                );
            }
            self.last_write.set(time);
        }
    }

    // for TimeInterval measurements, we increment the histogram with duration of event
    fn record_time_interval(&self, start: u64, stop: u64) {
        if self.source == Source::TimeInterval {
            self.counter.add(1);
            let duration = stop - start;
            if let Some(ref histogram) = self.histogram {
                histogram.increment(duration, <T as AtomicPrimitive>::Primitive::from(1_u8));
            }
            // track point of largest interval
            if self.max.time() > 0 {
                if duration > self.max.value() {
                    self.max.set(duration, start);
                }
            } else {
                self.max.set(duration, start);
            }
            // track point of smallest interval
            if self.min.time() > 0 {
                if duration < self.min.value() {
                    self.min.set(duration, start);
                }
            } else {
                self.min.set(duration, start);
            }
        }
    }

    pub fn counter(&self) -> u64 {
        self.counter.get()
    }

    pub fn percentile(&self, percentile: f64) -> Option<u64> {
        if let Some(ref histogram) = self.histogram {
            histogram.percentile(percentile)
        } else {
            None
        }
    }

    pub fn add_output(&self, output: Output) {
        let mut outputs = self.outputs.lock().unwrap();
        outputs.insert(output);
    }

    pub fn delete_output(&self, output: Output) {
        let mut outputs = self.outputs.lock().unwrap();
        outputs.remove(&output);
    }

    pub fn latch(&self) {
        if self.latched {
            if let Some(ref histogram) = self.histogram {
                histogram.clear();
            }
        }
        self.max.set(0, 0);
        self.min.set(0, 0);
    }

    pub fn zero(&self) {
        self.has_data.store(false, Ordering::SeqCst);
        self.last_write.set(0);
        self.counter.set(0);
        if let Some(ref histogram) = self.histogram {
            histogram.clear();
        }
        self.max.set(0, 0);
        self.min.set(0, 0);
    }

    pub fn readings(&self) -> Vec<Reading> {
        let mut result = Vec::new();
        let outputs = self.outputs.lock().unwrap();
        for output in &*outputs {
            match output {
                Output::Counter => {
                    result.push(Reading::new(self.name(), output.clone(), self.counter()));
                }
                Output::MaxPointTime => {
                    if self.max.time() > 0 {
                        result.push(Reading::new(self.name(), output.clone(), self.max.time()));
                    }
                }
                Output::MinPointTime => {
                    if self.max.time() > 0 {
                        result.push(Reading::new(self.name(), output.clone(), self.min.time()));
                    }
                }
                Output::Percentile(percentile) => {
                    if let Some(value) = self.percentile(percentile.as_f64()) {
                        result.push(Reading::new(self.name(), output.clone(), value));
                    }
                }
            }
        }
        result
    }

    pub fn hash_map(&self) -> HashMap<Output, u64> {
        let mut result = HashMap::new();
        let outputs = self.outputs.lock().unwrap();
        for output in &*outputs {
            match output {
                Output::Counter => {
                    result.insert(output.clone(), self.counter());
                }
                Output::MaxPointTime => {
                    if self.max.time() > 0 {
                        result.insert(output.clone(), self.max.time());
                    }
                }
                Output::MinPointTime => {
                    if self.max.time() > 0 {
                        result.insert(output.clone(), self.min.time());
                    }
                }
                Output::Percentile(percentile) => {
                    if let Some(value) = self.percentile(percentile.as_f64()) {
                        result.insert(output.clone(), value);
                    }
                }
            }
        }
        result
    }
}

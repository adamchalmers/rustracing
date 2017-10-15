//! Span.
use std::borrow::Cow;
use std::io::{Read, Write};
use std::ops::Deref;
use std::sync::mpsc;
use std::time::SystemTime;

use {Result, Tracer};
use carrier;
use convert::MaybeAsRef;
use log::{Log, LogBuilder};
use sampler::Sampler;
use tag::{Tag, TagValue};

/// Finished span receiver.
pub type SpanReceiver<T> = mpsc::Receiver<FinishedSpan<T>>;

/// Span.
///
/// When this span is dropped, it will be converted to `FinishedSpan` and
/// it will be sent to the associated `SpanReceiver`.
#[derive(Debug)]
pub struct Span<T>(Option<SpanInner<T>>);
impl<T> Span<T> {
    /// Returns `true` if this span is sampled (i.e., being traced).
    pub fn is_sampled(&self) -> bool {
        self.0.is_some()
    }

    /// Returns the context of this span.
    pub fn context(&self) -> Option<&SpanContext<T>> {
        self.0.as_ref().map(|x| &x.context)
    }

    /// Sets the operation name of this span.
    pub fn set_operation_name<F, N>(&mut self, f: F)
    where
        F: FnOnce() -> N,
        N: Into<Cow<'static, str>>,
    {
        if let Some(inner) = self.0.as_mut() {
            inner.operation_name = f().into();
        }
    }

    /// Sets the finish time of this span.
    pub fn set_finish_time<F>(&mut self, f: F)
    where
        F: FnOnce() -> SystemTime,
    {
        if let Some(inner) = self.0.as_mut() {
            inner.finish_time = Some(f());
        }
    }

    /// Sets the tag to this span.
    pub fn set_tag<F>(&mut self, f: F)
    where
        F: FnOnce() -> Tag,
    {
        if let Some(inner) = self.0.as_mut() {
            let tag = f();
            inner.tags.retain(|x| x.name() != tag.name());
            inner.tags.push(tag);
        }
    }

    /// Sets the baggage item to this span.
    pub fn set_baggage_item<F>(&mut self, f: F)
    where
        F: FnOnce() -> BaggageItem,
    {
        if let Some(inner) = self.0.as_mut() {
            let item = f();
            inner.context.baggage_items.retain(|x| x.name != item.name);
            inner.context.baggage_items.push(item);
        }
    }

    /// Gets the baggage item that has the name `name`.
    pub fn get_baggage_item(&self, name: &str) -> Option<&BaggageItem> {
        if let Some(inner) = self.0.as_ref() {
            inner.context.baggage_items.iter().find(|x| x.name == name)
        } else {
            None
        }
    }

    /// Logs structured data.
    pub fn log<F>(&mut self, f: F)
    where
        F: FnOnce(&mut LogBuilder),
    {
        if let Some(inner) = self.0.as_mut() {
            let mut builder = LogBuilder::new();
            f(&mut builder);
            if let Some(log) = builder.finish() {
                inner.logs.push(log);
            }
        }
    }

    pub(crate) fn new(
        operation_name: Cow<'static, str>,
        start_time: SystemTime,
        references: Vec<SpanReference<T>>,
        tags: Vec<Tag>,
        state: T,
        baggage_items: Vec<BaggageItem>,
        span_tx: mpsc::Sender<FinishedSpan<T>>,
    ) -> Self {
        let context = SpanContext::new(state, baggage_items);
        let inner = SpanInner {
            operation_name,
            start_time,
            finish_time: None,
            references,
            tags,
            logs: Vec::new(),
            context,
            span_tx,
        };
        Span(Some(inner))
    }
}
impl<T> Drop for Span<T> {
    fn drop(&mut self) {
        if let Some(inner) = self.0.take() {
            let finished = FinishedSpan {
                operation_name: inner.operation_name,
                start_time: inner.start_time,
                finish_time: inner.finish_time.unwrap_or_else(SystemTime::now),
                references: inner.references,
                tags: inner.tags,
                logs: inner.logs,
                context: inner.context,
            };
            let _ = inner.span_tx.send(finished);
        }
    }
}
impl<T> MaybeAsRef<SpanContext<T>> for Span<T> {
    fn maybe_as_ref(&self) -> Option<&SpanContext<T>> {
        self.context()
    }
}

#[derive(Debug)]
struct SpanInner<T> {
    operation_name: Cow<'static, str>,
    start_time: SystemTime,
    finish_time: Option<SystemTime>,
    references: Vec<SpanReference<T>>,
    tags: Vec<Tag>,
    logs: Vec<Log>,
    context: SpanContext<T>,
    span_tx: mpsc::Sender<FinishedSpan<T>>,
}

/// Finished span.
#[derive(Debug)]
pub struct FinishedSpan<T> {
    operation_name: Cow<'static, str>,
    start_time: SystemTime,
    finish_time: SystemTime,
    references: Vec<SpanReference<T>>,
    tags: Vec<Tag>,
    logs: Vec<Log>,
    context: SpanContext<T>,
}
impl<T> FinishedSpan<T> {
    /// Returns the operation name of this span.
    pub fn operation_name(&self) -> &str {
        self.operation_name.as_ref()
    }

    /// Returns the start time of this span.
    pub fn start_time(&self) -> SystemTime {
        self.start_time
    }

    /// Returns the finish time of this span.
    pub fn finish_time(&self) -> SystemTime {
        self.finish_time
    }

    /// Returns the logs recorded during this span.
    pub fn logs(&self) -> &[Log] {
        &self.logs
    }

    /// Returns the tags of this span.
    pub fn tags(&self) -> &[Tag] {
        &self.tags
    }

    /// Returns the references of this span.
    pub fn references(&self) -> &[SpanReference<T>] {
        &self.references
    }

    /// Returns the context of this span.
    pub fn context(&self) -> &SpanContext<T> {
        &self.context
    }
}

/// Span context.
///
/// Each `SpanContext` encapsulates the following state:
///
/// - `T`: OpenTracing-implementation-dependent state (for example, trace and span ids) needed to refer to a distinct `Span` across a process boundary
/// - `BaggageItems`: These are just key:value pairs that cross process boundaries
#[derive(Debug, Clone)]
pub struct SpanContext<T> {
    state: T,
    baggage_items: Vec<BaggageItem>,
}
impl<T> SpanContext<T> {
    /// Returns the baggage items associated with this context.
    pub fn baggage_items(&self) -> &[BaggageItem] {
        &self.baggage_items
    }

    /// Injects this context to the **Text Map** `carrier`.
    pub fn inject_to_text_map<C>(&self, carrier: &mut C) -> Result<()>
    where
        C: carrier::TextMap,
        T: carrier::InjectToTextMap<C>,
    {
        track!(T::inject_to_text_map(self, carrier))
    }

    /// Injects this context to the **HTTP Header** `carrier`.
    pub fn inject_to_http_header<C>(&self, carrier: &mut C) -> Result<()>
    where
        C: carrier::SetHttpHeaderField,
        T: carrier::InjectToHttpHeader<C>,
    {
        track!(T::inject_to_http_header(self, carrier))
    }

    /// Injects this context to the **Binary** `carrier`.
    pub fn inject_to_binary<C>(&self, carrier: &mut C) -> Result<()>
    where
        C: Write,
        T: carrier::InjectToBinary<C>,
    {
        track!(T::inject_to_binary(self, carrier))
    }

    /// Extracts a context from the **Text Map** `carrier`.
    pub fn extract_from_text_map<C>(carrier: &C) -> Result<Option<Self>>
    where
        C: carrier::TextMap,
        T: carrier::ExtractFromTextMap<C>,
    {
        track!(T::extract_from_text_map(carrier))
    }

    /// Extracts a context from the **HTTP Header** `carrier`.
    pub fn extract_from_http_header<C>(carrier: &C) -> Result<Option<Self>>
    where
        C: carrier::GetHttpHeaderField,
        T: carrier::ExtractFromHttpHeader<C>,
    {
        track!(T::extract_from_http_header(carrier))
    }

    /// Extracts a context from the **Binary** `carrier`.
    pub fn extract_from_binary<C>(carrier: &mut C) -> Result<Option<Self>>
    where
        C: Read,
        T: carrier::ExtractFromBinary<C>,
    {
        track!(T::extract_from_binary(carrier))
    }

    pub(crate) fn new(state: T, baggage_items: Vec<BaggageItem>) -> Self {
        SpanContext {
            state,
            baggage_items,
        }
    }
}
impl<T> Deref for SpanContext<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.state
    }
}
impl<T> AsRef<T> for SpanContext<T> {
    fn as_ref(&self) -> &T {
        &self.state
    }
}
impl<T> MaybeAsRef<SpanContext<T>> for SpanContext<T> {
    fn maybe_as_ref(&self) -> Option<&Self> {
        Some(self)
    }
}

/// Baggage item.
///
/// `BaggageItem`s are key:value string pairs that apply to a `Span`, its `SpanContext`,
/// and all `Span`s which directly or transitively reference the local `Span`.
/// That is, `BaggageItem`s propagate in-band along with the trace itself.
///
/// `BaggageItem`s enable powerful functionality given a full-stack OpenTracing integration
/// (for example, arbitrary application data from a mobile app can make it, transparently,
/// all the way into the depths of a storage system),
/// and with it some powerful costs: use this feature with care.
///
/// Use this feature thoughtfully and with care.
/// Every key and value is copied into every local and remote child of the associated `Span`,
/// and that can add up to a lot of network and cpu overhead.
#[derive(Debug, Clone)]
pub struct BaggageItem {
    name: String,
    value: String,
}
impl BaggageItem {
    /// Makes a new `BaggageItem` instance.
    pub fn new(name: &str, value: &str) -> Self {
        BaggageItem {
            name: name.to_owned(),
            value: value.to_owned(),
        }
    }

    /// Returns the name of this item.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the value of this item.
    pub fn value(&self) -> &str {
        &self.value
    }
}

/// Span reference.
#[derive(Debug, Clone)]
#[allow(missing_docs)]
pub enum SpanReference<T> {
    ChildOf(T),
    FollowsFrom(T),
}
impl<T> SpanReference<T> {
    /// Returns `true` if this is a `ChildOf` reference.
    pub fn is_child_of(&self) -> bool {
        if let SpanReference::ChildOf(_) = *self {
            true
        } else {
            false
        }
    }

    /// Returns `true` if this is a `FollowsFrom` reference.
    pub fn is_follows_from(&self) -> bool {
        if let SpanReference::FollowsFrom(_) = *self {
            true
        } else {
            false
        }
    }
}
impl<T> Deref for SpanReference<T> {
    type Target = T;
    fn deref(&self) -> &T {
        match *self {
            SpanReference::ChildOf(ref x) |
            SpanReference::FollowsFrom(ref x) => x,
        }
    }
}
impl<T> AsRef<T> for SpanReference<T> {
    fn as_ref(&self) -> &T {
        self.deref()
    }
}

/// Candidate span for tracing.
#[derive(Debug)]
pub struct CandidateSpan<'a, T: 'a> {
    tags: &'a [Tag],
    references: &'a [SpanReference<T>],
    baggage_items: &'a [BaggageItem],
}
impl<'a, T: 'a> CandidateSpan<'a, T> {
    /// Returns the tags of this span.
    pub fn tags(&self) -> &[Tag] {
        self.tags
    }

    /// Returns the references of this span.
    pub fn references(&self) -> &[SpanReference<T>] {
        self.references
    }

    /// Returns the baggage items of this span.
    pub fn baggage_items(&self) -> &[BaggageItem] {
        self.baggage_items
    }
}

/// Options for starting a span.
#[derive(Debug)]
pub struct StartSpanOptions<'a, S: 'a, T: 'a> {
    tracer: &'a Tracer<S, T>,
    operation_name: Cow<'static, str>,
    start_time: Option<SystemTime>,
    tags: Vec<Tag>,
    references: Vec<SpanReference<T>>,
    baggage_items: Vec<BaggageItem>,
}
impl<'a, S: 'a, T: 'a> StartSpanOptions<'a, S, T>
where
    S: Sampler<T>,
{
    /// Sets the start time of this span.
    pub fn start_time(mut self, time: SystemTime) -> Self {
        self.start_time = Some(time);
        self
    }

    /// Sets the tag to this span.
    pub fn tag(mut self, tag: Tag) -> Self {
        self.tags.push(tag);
        self
    }

    /// Adds the `ChildOf` reference to this span.
    pub fn child_of<C>(mut self, context: &C) -> Self
    where
        C: MaybeAsRef<SpanContext<T>>,
        T: Clone,
    {
        if let Some(context) = context.maybe_as_ref() {
            let reference = SpanReference::ChildOf(context.as_ref().clone());
            self.references.push(reference);
            self.baggage_items.extend(
                context.baggage_items().iter().cloned(),
            );
        }
        self
    }

    /// Adds the `FollowsFrom` reference to this span.
    pub fn follows_from<C>(mut self, context: &C) -> Self
    where
        C: MaybeAsRef<SpanContext<T>>,
        T: Clone,
    {
        if let Some(context) = context.maybe_as_ref() {
            let reference = SpanReference::FollowsFrom(context.as_ref().clone());
            self.references.push(reference);
            self.baggage_items.extend(
                context.baggage_items().iter().cloned(),
            );
        }
        self
    }

    /// Starts a new span.
    pub fn start(mut self) -> Span<T>
    where
        T: for<'b> From<CandidateSpan<'b, T>>,
    {
        self.normalize();
        if !self.is_sampled() {
            return Span(None);
        }
        let context = T::from(self.span());
        Span::new(
            self.operation_name,
            self.start_time.unwrap_or_else(SystemTime::now),
            self.references,
            self.tags,
            context,
            self.baggage_items,
            self.tracer.span_tx(),
        )
    }

    /// Starts a new span with the explicit `context`.
    pub fn start_with_context(mut self, context: T) -> Span<T> {
        self.normalize();
        if !self.is_sampled() {
            return Span(None);
        }
        Span::new(
            self.operation_name,
            self.start_time.unwrap_or_else(SystemTime::now),
            self.references,
            self.tags,
            context,
            self.baggage_items,
            self.tracer.span_tx(),
        )
    }

    pub(crate) fn new<N>(tracer: &'a Tracer<S, T>, operation_name: N) -> Self
    where
        N: Into<Cow<'static, str>>,
    {
        StartSpanOptions {
            tracer,
            operation_name: operation_name.into(),
            start_time: None,
            tags: Vec::new(),
            references: Vec::new(),
            baggage_items: Vec::new(),
        }
    }

    fn normalize(&mut self) {
        self.tags.reverse();
        self.tags.sort_by(|a, b| a.name().cmp(b.name()));
        self.tags.dedup_by(|a, b| a.name() == b.name());

        self.baggage_items.reverse();
        self.baggage_items.sort_by(|a, b| a.name().cmp(b.name()));
        self.baggage_items.dedup_by(|a, b| a.name() == b.name());
    }

    fn span(&self) -> CandidateSpan<T> {
        CandidateSpan {
            references: &self.references,
            tags: &self.tags,
            baggage_items: &self.baggage_items,
        }
    }

    fn is_sampled(&self) -> bool {
        if let Some(&TagValue::Integer(n)) =
            self.tags
                .iter()
                .find(|t| t.name() == "sampling.priority")
                .map(|t| t.value())
        {
            n > 0
        } else {
            self.tracer.sampler().is_sampled(&self.span())
        }
    }
}

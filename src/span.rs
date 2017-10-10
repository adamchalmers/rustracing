use std::borrow::Cow;
use std::ops::Deref;
use std::sync::mpsc;
use std::time::SystemTime;

use convert::MaybeAsRef;
use log::{Log, LogBuilder};
use tag::Tag;

pub type SpanReceiver<T> = mpsc::Receiver<FinishedSpan<T>>;

// TODO: StartSpanOptions
#[derive(Debug)]
pub struct SpanBuilder<T> {
    start_time: Option<SystemTime>,
    tags: Vec<Tag>,
    references: Vec<SpanReference<T>>,
    baggage_items: Vec<BaggageItem>,
}
impl<T> SpanBuilder<T> {
    pub(crate) fn new() -> Self {
        SpanBuilder {
            start_time: None,
            tags: Vec::new(),
            references: Vec::new(),
            baggage_items: Vec::new(),
        }
    }
    pub fn start_time(&mut self, time: SystemTime) -> &mut Self {
        self.start_time = Some(time);
        self
    }
    pub fn tag(&mut self, tag: Tag) -> &mut Self {
        self.tags.push(tag);
        self
    }
    pub fn child_of<C>(&mut self, context: C) -> &mut Self
    where
        C: MaybeAsRef<SpanContext<T>>,
        T: Clone,
    {
        if let Some(context) = context.maybe_as_ref() {
            let reference = SpanReference::ChildOf(context.state().clone());
            self.references.push(reference);
            self.baggage_items.extend(
                context.baggage_items().iter().cloned(),
            );
        }
        self
    }
    pub fn follows_from<C>(&mut self, context: C) -> &mut Self
    where
        C: MaybeAsRef<SpanContext<T>>,
        T: Clone,
    {
        if let Some(context) = context.maybe_as_ref() {
            let reference = SpanReference::FollowsFrom(context.state().clone());
            self.references.push(reference);
            self.baggage_items.extend(
                context.baggage_items().iter().cloned(),
            );
        }
        self
    }
    pub(crate) fn finish<N>(mut self, operation_name: N) -> (InactiveSpan, Vec<SpanReference<T>>)
    where
        N: Into<Cow<'static, str>>,
    {
        self.tags.reverse();
        self.tags.sort_by(|a, b| a.key().cmp(b.key()));
        self.tags.dedup_by(|a, b| a.key() == b.key());

        self.baggage_items.reverse();

        (
            InactiveSpan {
                operation_name: operation_name.into(),
                start_time: self.start_time.unwrap_or_else(|| SystemTime::now()),
                tags: self.tags,
                references: self.references.len(),
                baggage_items: self.baggage_items,
            },
            self.references,
        )
    }
}

#[derive(Debug)]
pub struct InactiveSpan {
    operation_name: Cow<'static, str>,
    start_time: SystemTime,
    tags: Vec<Tag>,
    references: usize,
    baggage_items: Vec<BaggageItem>,
}
impl InactiveSpan {
    pub(crate) fn activate<T>(
        self,
        state: T,
        references: Vec<SpanReference<T>>,
        span_tx: mpsc::Sender<FinishedSpan<T>>,
    ) -> Span<T> {
        let context = SpanContext::new(state, self.baggage_items);
        let inner = SpanInner {
            operation_name: self.operation_name,
            start_time: self.start_time,
            finish_time: None,
            tags: self.tags,
            logs: Vec::new(),
            references: references,
            context,
            span_tx,
        };
        Span(Some(inner))
    }
    pub fn operation_name(&self) -> &str {
        self.operation_name.as_ref()
    }
    pub fn start_time(&self) -> SystemTime {
        self.start_time
    }
    pub fn tags(&self) -> &[Tag] {
        &self.tags
    }
    pub fn references(&self) -> usize {
        self.references
    }
    pub fn baggage_items(&self) -> &[BaggageItem] {
        &self.baggage_items
    }
}

#[derive(Debug)]
pub struct Span<T>(Option<SpanInner<T>>);
impl<T> Span<T> {
    pub fn disabled() -> Self {
        Span(None)
    }
    pub fn is_enabled(&self) -> bool {
        self.0.is_some()
    }
    pub fn context(&self) -> Option<&SpanContext<T>> {
        self.0.as_ref().map(|x| &x.context)
    }
    pub fn set_operation_name(&mut self, name: Cow<'static, str>) {
        if let Some(inner) = self.0.as_mut() {
            inner.operation_name = name.into();
        }
    }
    pub fn set_finish_time(&mut self, time: SystemTime) {
        if let Some(inner) = self.0.as_mut() {
            inner.finish_time = Some(time);
        }
    }
    pub fn set_tag(&mut self, tag: Tag) {
        if let Some(inner) = self.0.as_mut() {
            inner.tags.retain(|x| x.key() != tag.key());
            inner.tags.push(tag);
        }
    }
    pub fn set_baggage_item(&mut self, item: BaggageItem) {
        if let Some(inner) = self.0.as_mut() {
            inner.context.baggage_items.retain(|x| x.key != item.key);
            inner.context.baggage_items.push(item);
        }
    }
    pub fn get_baggage_item(&self, key: &str) -> Option<&BaggageItem> {
        if let Some(inner) = self.0.as_ref() {
            inner.context.baggage_items.iter().find(|x| x.key == key)
        } else {
            None
        }
    }
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
}
impl<T> Drop for Span<T> {
    fn drop(&mut self) {
        if let Some(inner) = self.0.take() {
            let finished = FinishedSpan {
                operation_name: inner.operation_name,
                start_time: inner.start_time,
                finish_time: inner.finish_time.unwrap_or_else(|| SystemTime::now()),
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
    pub fn operation_name(&self) -> &str {
        self.operation_name.as_ref()
    }
    pub fn start_time(&self) -> SystemTime {
        self.start_time
    }
    pub fn finish_time(&self) -> SystemTime {
        self.finish_time
    }
    pub fn logs(&self) -> &[Log] {
        &self.logs
    }
    pub fn tags(&self) -> &[Tag] {
        &self.tags
    }
    pub fn references(&self) -> &[SpanReference<T>] {
        &self.references
    }
    pub fn context(&self) -> &SpanContext<T> {
        &self.context
    }
}

#[derive(Debug, Clone)]
pub struct SpanContext<T> {
    state: T,
    baggage_items: Vec<BaggageItem>,
}
impl<T> SpanContext<T> {
    pub(crate) fn new(state: T, mut baggage_items: Vec<BaggageItem>) -> Self {
        baggage_items.sort_by(|a, b| a.key.cmp(&b.key));
        baggage_items.dedup_by(|a, b| a.key == b.key);
        SpanContext {
            state,
            baggage_items,
        }
    }
    pub fn state(&self) -> &T {
        &self.state
    }
    pub fn baggage_items(&self) -> &[BaggageItem] {
        &self.baggage_items
    }
}
impl<T> MaybeAsRef<SpanContext<T>> for SpanContext<T> {
    fn maybe_as_ref(&self) -> Option<&Self> {
        Some(self)
    }
}

#[derive(Debug, Clone)]
pub struct BaggageItem {
    pub key: String,
    pub value: String,
}

#[derive(Debug)]
pub enum SpanReference<T> {
    ChildOf(T),
    FollowsFrom(T),
}
impl<T> Deref for SpanReference<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        match *self {
            SpanReference::ChildOf(ref s) => s,
            SpanReference::FollowsFrom(ref s) => s,
        }
    }
}

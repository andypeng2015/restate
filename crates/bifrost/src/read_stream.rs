// Copyright (c) 2024 -  Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::pin::Pin;
use std::sync::Arc;
use std::task::ready;
use std::task::Poll;

use futures::stream::FusedStream;
use futures::Stream;
use pin_project::pin_project;
use restate_types::logs::{LogId, Lsn, SequenceNumber};

use crate::bifrost::BifrostInner;
use crate::loglet::LogletReadStreamWrapper;
use crate::loglet::LogletWrapper;
use crate::FindTailAttributes;
use crate::LogRecord;
use crate::Result;

#[pin_project]
pub struct LogReadStream {
    #[pin]
    current_loglet_stream: LogletReadStreamWrapper,
    current_loglet: LogletWrapper,
    inner: Arc<BifrostInner>,
    _last_known_tail: Lsn,
    log_id: LogId,
    // inclusive max lsn to read to
    until_lsn: Lsn,
    terminated: bool,
    /// Represents the _current_ record (or the last lsn that was returned from this stream).
    //  This is akin to the lsn that can be passed to `read_next_single(after)` to read the
    //  next record in the log.
    read_pointer: Lsn,
}

impl LogReadStream {
    pub(crate) async fn create(
        inner: Arc<BifrostInner>,
        log_id: LogId,
        after: Lsn,
        // Inclusive. Use Lsn::MAX for a tailing stream. Once reached, stream will terminate
        // (return Ready(None)).
        until_lsn: Lsn,
    ) -> Result<Self> {
        // todo: support switching loglets. At the moment, this is hard-wired to a single loglet
        // implementation.
        let current_loglet = inner
            // find the loglet where the _next_ lsn resides.
            .find_loglet_for_lsn(log_id, after.next())
            .await?;
        let (last_loglet, last_known_tail) = inner
            .find_tail(log_id, FindTailAttributes::default())
            .await?;
        debug_assert_eq!(last_loglet, current_loglet);

        let current_loglet_stream = current_loglet.create_wrapped_read_stream(after).await?;
        Ok(Self {
            current_loglet_stream,
            // reserved for future use
            current_loglet: last_loglet,
            // reserved for future use
            _last_known_tail: last_known_tail.unwrap_or(Lsn::INVALID),
            inner,
            log_id,
            read_pointer: after,
            until_lsn,
            terminated: false,
        })
    }

    pub fn is_terminated(&self) -> bool {
        self.terminated
    }

    pub fn read_pointer(&self) -> Lsn {
        self.read_pointer
    }

    fn calculate_read_pointer(record: &LogRecord) -> Lsn {
        match &record.record {
            // On trim gaps, we fast-forward the read pointer to the end of the gap. We do
            // this after delivering a TrimGap record. This means that the next read operation
            // skips over the boundary of the gap.
            crate::Record::TrimGap(trim_gap) => trim_gap.until,
            crate::Record::Data(_) => record.offset,
            crate::Record::Seal(_) => record.offset,
        }
    }

    /// Current read pointer. This is the LSN of the last read record, or the
    /// LSN that we will read "after" if we call `read_next`.
    pub fn current_read_pointer(&self) -> Lsn {
        self.read_pointer
    }
}

impl FusedStream for LogReadStream {
    fn is_terminated(&self) -> bool {
        self.terminated
    }
}

/// Read the next record from the log after the current read pointer. The stream will yield
/// after the record is available to read, this will async-block indefinitely if no records are
/// ever written to the log beyond the read pointer.
impl Stream for LogReadStream {
    type Item = Result<LogRecord>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        if self.read_pointer >= self.until_lsn {
            self.as_mut().terminated = true;
            return Poll::Ready(None);
        }
        // Are we after the known tail?
        // todo: refresh the tail (in a multi-loglet universe)
        let maybe_record = ready!(self
            .as_mut()
            .project()
            .current_loglet_stream
            .as_mut()
            .poll_next(cx));
        match maybe_record {
            Some(Ok(record)) => {
                let record = record
                    .decode()
                    .expect("decoding a bifrost envelope succeeds");
                let new_pointer = Self::calculate_read_pointer(&record);
                debug_assert!(new_pointer > self.read_pointer);
                self.read_pointer = new_pointer;
                Poll::Ready(Some(Ok(record)))
            }
            Some(Err(e)) => Poll::Ready(Some(Err(e))),
            None => {
                // todo: check if we should switch the loglet.
                self.as_mut().terminated = true;
                Poll::Ready(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {

    use std::sync::atomic::AtomicUsize;

    use crate::{Bifrost, Record, TrimGap};

    use super::*;
    use googletest::prelude::*;

    use restate_core::{TaskKind, TestCoreEnvBuilder};
    use restate_rocksdb::RocksDbManager;
    use restate_types::arc_util::Constant;
    use restate_types::config::CommonOptions;
    use restate_types::logs::metadata::ProviderKind;
    use tokio_stream::StreamExt;
    use tracing::info;
    use tracing_test::traced_test;

    use restate_types::logs::Payload;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[traced_test]
    async fn test_basic_readstream() -> anyhow::Result<()> {
        // Make sure that panics exits the process.
        let orig_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic_info| {
            // invoke the default handler and exit the process
            orig_hook(panic_info);
            std::process::exit(1);
        }));

        let node_env = TestCoreEnvBuilder::new_with_mock_network()
            .set_provider_kind(ProviderKind::Local)
            .build()
            .await;

        let tc = node_env.tc;
        tc.run_in_scope("test", None, async {
            RocksDbManager::init(Constant::new(CommonOptions::default()));

            let read_after = Lsn::from(5);
            let mut bifrost = Bifrost::init().await;

            let log_id = LogId::from(0);
            let mut reader = bifrost.create_reader(log_id, read_after, Lsn::MAX).await?;

            let tail = bifrost
                .find_tail(log_id, FindTailAttributes::default())
                .await?;
            // no records have been written
            assert!(tail.is_none());
            assert_eq!(read_after, reader.current_read_pointer());

            let read_counter = Arc::new(AtomicUsize::new(0));
            // spawn a reader that reads 5 records and exits.
            let counter_clone = read_counter.clone();
            let id = tc.spawn(TaskKind::TestRunner, "read-records", None, async move {
                for i in 1..=5 {
                    let record = reader.next().await.expect("to never terminate")?;
                    let expected_lsn = Lsn::from(i) + read_after;
                    assert_eq!(expected_lsn, reader.current_read_pointer());
                    counter_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    assert_eq!(expected_lsn, record.offset);
                    assert_eq!(
                        Payload::new(format!("record{}", expected_lsn)).body(),
                        record.record.into_payload_unchecked().body()
                    );
                    assert_eq!(expected_lsn, reader.current_read_pointer());
                }
                Ok(())
            })?;

            let reader_bg_handle = tc.take_task(id).expect("read-records task to exist");

            tokio::task::yield_now().await;
            // Not finished, we still didn't append records
            assert!(!reader_bg_handle.is_finished());

            // append 5 records to the log
            for i in 1..=5 {
                let lsn = bifrost
                    .append(LogId::from(0), Payload::new(format!("record{}", i)))
                    .await?;
                info!(?lsn, "appended record");
            }

            // Written records are not enough for the reader to finish.
            // Not finished, we still didn't append records
            tokio::task::yield_now().await;
            assert!(!reader_bg_handle.is_finished());
            assert!(read_counter.load(std::sync::atomic::Ordering::Relaxed) == 0);

            // write 5 more records.
            for i in 6..=10 {
                bifrost
                    .append(LogId::from(0), Payload::new(format!("record{}", i)))
                    .await?;
            }

            // reader has finished
            reader_bg_handle.await?;
            assert!(read_counter.load(std::sync::atomic::Ordering::Relaxed) == 5);

            anyhow::Ok(())
        })
        .await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[traced_test]
    async fn test_read_stream_with_trim() -> anyhow::Result<()> {
        // Make sure that panics exits the process.
        let orig_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic_info| {
            // invoke the default handler and exit the process
            orig_hook(panic_info);
            std::process::exit(1);
        }));

        let node_env = TestCoreEnvBuilder::new_with_mock_network()
            .set_provider_kind(ProviderKind::Local)
            .build()
            .await;
        node_env
            .tc
            .run_in_scope("test", None, async {
                RocksDbManager::init(Constant::new(CommonOptions::default()));

                let log_id = LogId::from(0);
                let mut bifrost = Bifrost::init().await;

                assert!(bifrost.get_trim_point(log_id).await?.is_none());

                for _ in 1..=10 {
                    bifrost.append(log_id, Payload::default()).await?;
                }

                bifrost.trim(log_id, Lsn::from(5)).await?;

                assert_eq!(
                    Some(Lsn::from(10)),
                    bifrost
                        .find_tail(log_id, FindTailAttributes::default())
                        .await?,
                );
                assert_eq!(Some(Lsn::from(5)), bifrost.get_trim_point(log_id).await?);

                let mut read_stream = bifrost
                    .create_reader(log_id, Lsn::INVALID, Lsn::MAX)
                    .await?;

                let record = read_stream.next().await.unwrap()?;
                assert_that!(
                    record,
                    pat!(LogRecord {
                        offset: eq(Lsn::from(1)),
                        record: pat!(Record::TrimGap(pat!(TrimGap {
                            until: eq(Lsn::from(5)),
                        })))
                    })
                );

                for lsn in 5..7 {
                    let record = read_stream.next().await.unwrap()?;
                    assert_that!(
                        record,
                        pat!(LogRecord {
                            offset: eq(Lsn::from(lsn + 1)),
                            record: pat!(Record::Data(_))
                        })
                    );
                }
                assert!(!read_stream.is_terminated());
                assert_eq!(Lsn::from(7), read_stream.read_pointer());

                // trimming beyond the release point will fall back to the release point
                bifrost.trim(log_id, Lsn::from(u64::MAX)).await?;
                assert_eq!(bifrost.get_trim_point(log_id).await?, Some(Lsn::from(10)));

                for _ in 0..10 {
                    bifrost.append(log_id, Payload::default()).await?;
                }

                // read stream should send a gap from 8->10
                let record = read_stream.next().await.unwrap()?;
                assert_that!(
                    record,
                    pat!(LogRecord {
                        offset: eq(Lsn::from(8)),
                        record: pat!(Record::TrimGap(pat!(TrimGap {
                            until: eq(Lsn::from(10)),
                        })))
                    })
                );

                for lsn in 10..20 {
                    let record = read_stream.next().await.unwrap()?;
                    assert_that!(
                        record,
                        pat!(LogRecord {
                            offset: eq(Lsn::from(lsn + 1)),
                            record: pat!(Record::Data(_))
                        })
                    );
                }
                // we are at tail. polling should return pending.
                let pinned = std::pin::pin!(read_stream.next());
                let next_is_pending = futures::poll!(pinned);
                assert!(matches!(next_is_pending, Poll::Pending));

                Ok(())
            })
            .await
    }
}

use crate::Error;
use std::{
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetentionTestEvent {
    Started,
    Success,
    Failure(Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetentionPauseStage {
    BeforeClock,
    BeforeCommit,
}

pub(crate) struct RetentionTestPause {
    pub(crate) stage: RetentionPauseStage,
    pub(crate) entered: oneshot::Sender<()>,
    pub(crate) release: oneshot::Receiver<()>,
}

impl RetentionTestPause {
    pub(crate) async fn wait(self) -> Result<(), Error> {
        self.entered.send(()).map_err(|_| Error::Storage)?;
        self.release.await.map_err(|_| Error::Storage)
    }
}

pub(crate) struct RetentionTestControl {
    pub(crate) clock: Arc<AtomicI64>,
    pub(crate) interval: Duration,
    pub(crate) retention_seconds: u64,
    pub(crate) budget_bytes: u64,
    pub(crate) events: mpsc::UnboundedSender<RetentionTestEvent>,
    pub(crate) pause: Option<RetentionTestPause>,
}

impl RetentionTestControl {
    pub(crate) async fn before_clock(&mut self) -> Result<(), Error> {
        if self
            .pause
            .as_ref()
            .is_some_and(|pause| pause.stage == RetentionPauseStage::BeforeClock)
        {
            self.pause
                .take()
                .ok_or(Error::InvalidFormat)?
                .wait()
                .await?;
        }
        Ok(())
    }

    pub(crate) fn take_before_commit(&mut self) -> Option<RetentionTestPause> {
        if self
            .pause
            .as_ref()
            .is_some_and(|pause| pause.stage == RetentionPauseStage::BeforeCommit)
        {
            self.pause.take()
        } else {
            None
        }
    }

    pub(crate) fn now_seconds(&self) -> i64 {
        self.clock.load(Ordering::SeqCst)
    }
}

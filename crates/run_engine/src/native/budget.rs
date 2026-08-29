use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result as AnyResult, bail};
use oci_spec::image::Descriptor;

use crate::{ContentError, ContentErrorKind, OciContent, OciContentStore};

#[derive(Clone, Copy)]
pub(super) struct OperationBudget {
    deadline: Instant,
    operation: &'static str,
}

impl OperationBudget {
    pub(super) fn new(duration: Duration, operation: &'static str) -> AnyResult<Self> {
        let deadline = Instant::now()
            .checked_add(duration)
            .with_context(|| format!("{operation} exceeds the monotonic clock range"))?;
        Ok(Self {
            deadline,
            operation,
        })
    }

    pub(super) fn check(self) -> AnyResult<()> {
        if Instant::now() >= self.deadline {
            bail!("{} deadline exceeded", self.operation);
        }
        Ok(())
    }

    pub(super) fn remaining(self) -> AnyResult<Duration> {
        self.check()?;
        Ok(self.deadline.saturating_duration_since(Instant::now()))
    }

    pub(super) const fn deadline(self) -> Instant {
        self.deadline
    }

    fn check_io(self) -> std::io::Result<()> {
        if Instant::now() >= self.deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("{} deadline exceeded", self.operation),
            ));
        }
        Ok(())
    }

    fn check_content(self) -> Result<(), ContentError> {
        if Instant::now() >= self.deadline {
            return Err(ContentError::new(
                ContentErrorKind::Internal,
                format!("{} deadline exceeded", self.operation),
            ));
        }
        Ok(())
    }
}

pub(super) struct BudgetedStore {
    inner: Arc<dyn OciContentStore>,
    budget: OperationBudget,
}

impl BudgetedStore {
    pub(super) fn new(inner: Arc<dyn OciContentStore>, budget: OperationBudget) -> Self {
        Self { inner, budget }
    }
}

impl OciContentStore for BudgetedStore {
    fn published_content_is_immutable(&self) -> bool {
        self.inner.published_content_is_immutable()
    }

    fn open(&self, descriptor: &Descriptor) -> Result<Box<dyn OciContent>, ContentError> {
        self.budget.check_content()?;
        let content = self.inner.open(descriptor)?;
        self.budget.check_content()?;
        Ok(Box::new(BudgetedContent {
            inner: content,
            budget: self.budget,
        }))
    }

    fn publish(&self, descriptor: &Descriptor, content: &mut dyn Read) -> Result<(), ContentError> {
        self.budget.check_content()?;
        let mut content = BudgetedRead {
            inner: content,
            budget: self.budget,
        };
        self.inner.publish(descriptor, &mut content)?;
        self.budget.check_content()
    }
}

struct BudgetedContent {
    inner: Box<dyn OciContent>,
    budget: OperationBudget,
}

impl Read for BudgetedContent {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.budget.check_io()?;
        let count = self.inner.read(buffer)?;
        self.budget.check_io()?;
        Ok(count)
    }
}

impl Seek for BudgetedContent {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.budget.check_io()?;
        let offset = self.inner.seek(position)?;
        self.budget.check_io()?;
        Ok(offset)
    }
}

struct BudgetedRead<'a> {
    inner: &'a mut dyn Read,
    budget: OperationBudget,
}

impl Read for BudgetedRead<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.budget.check_io()?;
        let count = self.inner.read(buffer)?;
        self.budget.check_io()?;
        Ok(count)
    }
}

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result as AnyResult;
use oci_spec::image::{Descriptor, MediaType};
use run_protocol::ImageDescriptor;

use super::budget::{BudgetedStore, OperationBudget};
use super::prepare::PreparedProgram;
use crate::OciContentStore;
use crate::oci::{publish_expected, publish_final_image};
use crate::rootfs::CapturedLayer;

pub(super) enum CaptureFailure {
    Deadline(String),
    Capture(String),
}

impl CaptureFailure {
    pub(super) fn operation_message(&self) -> &str {
        match self {
            Self::Deadline(message) | Self::Capture(message) => message,
        }
    }

    pub(super) fn unavailable_reason(&self) -> String {
        match self {
            Self::Deadline(_) => {
                "the final environment capture deadline could not be established".to_owned()
            }
            Self::Capture(message) => message.clone(),
        }
    }
}

pub(super) fn capture(
    store: Arc<dyn OciContentStore>,
    timeout: Duration,
    program: &PreparedProgram,
) -> Result<ImageDescriptor, CaptureFailure> {
    let budget = OperationBudget::new(timeout, "final environment capture").map_err(|error| {
        CaptureFailure::Deadline(format!(
            "failed to establish final environment capture deadline: {error:#}"
        ))
    })?;
    let store = BudgetedStore::new(store, budget);
    let result = (|| -> AnyResult<ImageDescriptor> {
        budget.check()?;
        program.rootfs.ensure_no_mounts()?;
        budget.check()?;
        let captured = program.rootfs.capture()?;
        budget.check()?;
        let image =
            publish_capture(&store, &program.parent, &captured).map_err(anyhow::Error::from)?;
        budget.check()?;
        Ok(image)
    })();
    result.map_err(|error| {
        CaptureFailure::Capture(format!("failed to capture final environment: {error:#}"))
    })
}

fn publish_capture(
    store: &dyn OciContentStore,
    parent: &ImageDescriptor,
    captured: &CapturedLayer,
) -> Result<ImageDescriptor, crate::oci::OciError> {
    let descriptor = Descriptor::new(
        captured.media_type.clone(),
        captured.size,
        captured.diff_id.clone(),
    );
    let mut reader = captured.open().map_err(|error| crate::oci::OciError::Io {
        path: "final.layer".to_owned(),
        source: std::io::Error::other(error.to_string()),
    })?;
    publish_expected(
        store,
        &descriptor,
        &mut reader,
        &[MediaType::ImageLayer],
        "final.layer",
    )?;
    publish_final_image(store, parent, Some((descriptor, captured.diff_id.clone())))
}

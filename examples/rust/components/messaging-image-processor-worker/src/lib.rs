use std::io::Cursor;

use anyhow::{anyhow, bail, Result};
use bytes::Bytes;
use image::{DynamicImage, ImageFormat, ImageReader};

mod bindings {
    //! These bindings are generated by wit-bindgen, and reused by other parts of the crate
    use crate::ImageProcessorWorker;
    wit_bindgen::generate!({ generate_all });
    export!(ImageProcessorWorker);
}

// NOTE: The imports below are generated by wit-bindgen
use bindings::exports::wasmcloud::messaging;
use bindings::wasi::blobstore::blobstore;
use bindings::wasi::http::types::{Fields, OutgoingRequest, Scheme};
use bindings::wasi::logging::logging::{log, Level};
use bindings::wasmcloud::messaging::types::BrokerMessage;
use bindings::wasmcloud::task_manager::tasks;
// --- END of wit-bindgen generated imports

mod http;

mod objstore;
use objstore::{read_object, write_object};

mod processing;
pub use processing::{
    BlobstorePath, ImageOperation, ImagePath, ImageProcessingRequest, JobMessage,
    DEFAULT_IMAGE_BYTES,
};

/// Maximum bytes to read at a time from the incoming request body
/// this value is chosen somewhat arbitrarily, and is not a limit for bytes read,
/// but is instead the amount of bytes to be read *at once*
const MAX_READ_BYTES: u32 = 2048;

/// Maximum bytes to write at a time, due to the limitations on wasi-io's blocking_write_and_flush()
const MAX_WRITE_BYTES: usize = 4096;

const LOG_CONTEXT: &str = "image-processor-worker";

const WORKER_ID: &str = "rust-component-worker";

/// All implementation of the WIT world (see wit/component.wit) hangs off of this struct
struct ImageProcessorWorker;

impl messaging::handler::Guest for ImageProcessorWorker {
    fn handle_message(msg: BrokerMessage) -> Result<(), String> {
        // Parse out the image processing request
        let (ipr, _task, lease_id) = match ImageProcessingRequest::from_task_msg(&msg) {
            Ok(r) => r,
            Err(e) => {
                log(
                    Level::Error,
                    LOG_CONTEXT,
                    &format!("parse image processing request: {e}"),
                );
                return Err("failed to parse image processing request".into());
            }
        };

        // Fetch the bytes from the request
        let image_bytes = match ipr.fetch_image() {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                log(Level::Error, LOG_CONTEXT, "fetch image failed, no bytes");
                return Err("fetch image failed (no bytes)".into());
            }
            Err(e) => {
                log(
                    Level::Error,
                    LOG_CONTEXT,
                    &format!("fetch image failed: {e}"),
                );
                return Err("fetch image failed".into());
            }
        };

        // Perform the transformations on the image
        let output_image = match transform_image(
            ipr.image_format.and_then(ImageFormat::from_mime_type),
            image_bytes,
            ipr.operations,
        ) {
            Ok(b) => b,
            Err(e) => {
                log(
                    Level::Error,
                    LOG_CONTEXT,
                    &format!("failed to transform image: {e}"),
                );
                return Err("failed to send transform image: {e}".into());
            }
        };
        let output_bytes = Bytes::from(output_image.into_bytes());

        // Write the transformed image to object storage
        if let ImagePath::Blobstore {
            path: BlobstorePath { bucket, key },
        } = ipr.destination
        {
            log(
                Level::Info,
                LOG_CONTEXT,
                &format!("writing to [{bucket}/{key}]"),
            );
            if let Err(e) = write_object(output_bytes.clone(), &bucket, &key) {
                log(
                    Level::Error,
                    LOG_CONTEXT,
                    &format!("writing object failed: {e}"),
                );
                let _ = tasks::mark_task_failed(
                    &lease_id,
                    &String::from(WORKER_ID),
                    &String::from("failed to output to object storage"),
                );
                log(
                    Level::Error,
                    LOG_CONTEXT,
                    &format!("object storage write failed: {e}"),
                );
                return Err("object storage write failed".into());
            }
        }

        // Mark the task complete
        match tasks::mark_task_completed(&lease_id, &String::from(WORKER_ID)) {
            Ok(()) => (),
            Err(e) => {
                log(
                    Level::Error,
                    LOG_CONTEXT,
                    &format!("failed to retrieve task: {e}"),
                );
                return Err("failed to retrieve task".into());
            }
        };

        Ok(())
    }
}

/// Perform one or more provided operations on a given image
pub(crate) fn transform_image(
    content_type: Option<ImageFormat>,
    image_bytes: Bytes,
    operations: Vec<ImageOperation>,
) -> Result<DynamicImage> {
    let cursor = Cursor::new(image_bytes);
    let reader = if let Some(ct) = content_type {
        ImageReader::with_format(cursor, ct)
    } else {
        ImageReader::new(cursor)
            .with_guessed_format()
            .map_err(|e| anyhow!("failed to guess format: {e}"))?
    };
    let mut image = reader
        .decode()
        .map_err(|e| anyhow!("failed to decode image: {e}"))?;

    for op in operations {
        log(
            Level::Info,
            LOG_CONTEXT,
            format!("performing operation [{op:?}]").as_str(),
        );
        match op {
            ImageOperation::NoOp => {
                continue;
            }
            ImageOperation::Grayscale => {
                image = image.grayscale();
            }
            ImageOperation::Resize {
                height_px,
                width_px,
            } => {
                image = image.resize(width_px, height_px, image::imageops::FilterType::Nearest);
            }
        }
    }

    Ok(image)
}

/// Utility trait to enable types to be constructed from MIME types (ex. `image/jpeg`)
///
/// This is primarily used to extend [`ImageFormat`]
#[allow(dead_code)]
trait FromMimeType
where
    Self: Sized,
{
    fn from_mime_type(s: &str) -> Result<Self>;
}

impl FromMimeType for ImageFormat {
    fn from_mime_type(s: &str) -> Result<ImageFormat> {
        match s {
            "image/jpeg" => Ok(ImageFormat::Jpeg),
            "image/tiff" => Ok(ImageFormat::Tiff),
            "image/webp" => Ok(ImageFormat::WebP),
            "image/gif" => Ok(ImageFormat::Gif),
            "image/bmp" => Ok(ImageFormat::Bmp),
            "image/png" => Ok(ImageFormat::Png),
            _ => bail!("unrecognized format [{s}]"),
        }
    }
}

impl ImageProcessingRequest {
    /// Fetch the bytes that make up the image
    pub(crate) fn fetch_image(&self) -> Result<Option<Bytes>> {
        match &self.source {
            ImagePath::DefaultImage => Ok(Some(Bytes::from(DEFAULT_IMAGE_BYTES))),
            ImagePath::Attached => Ok(self.image_data.clone()),
            ImagePath::RemoteHttps { url } => {
                let req = OutgoingRequest::new(Fields::new());
                req.set_scheme(Some(&Scheme::Https))
                    .map_err(|()| anyhow!("failed to set scheme"))?;
                req.set_authority(Some(url.authority()))
                    .map_err(|()| anyhow!("failed to set authority"))?;
                req.set_path_with_query(Some(url.path()))
                    .map_err(|()| anyhow!("failed to set path and query"))?;
                req.fetch_bytes()
            }
            ImagePath::Blobstore {
                path: BlobstorePath { bucket, key },
            } => read_object(bucket, key).map(Option::Some),
        }
    }
}

use milli::Object;
use serde::Serialize;
use time::{Duration, OffsetDateTime};
use utoipa::ToSchema;

use crate::error::ResponseError;
use crate::settings::{Settings, Unchecked};
use crate::tasks::{serialize_duration, Details, IndexSwap, Kind, Status, Task, TaskId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
#[schema(rename_all = "camelCase")]
pub struct TaskView {
    /// The unique sequential identifier of the task.
    #[schema(value_type = u32, example = 4312)]
    pub uid: TaskId,
    /// The unique identifier of the index where this task is operated.
    #[schema(example = json!("movies"))]
    #[serde(default)]
    pub index_uid: Option<String>,
    pub status: Status,
    /// The type of the task.
    #[serde(rename = "type")]
    pub kind: Kind,
    /// The uid of the task that performed the taskCancelation if the task has been canceled.
    #[schema(value_type = Option<u32>, example = json!(4326))]
    pub canceled_by: Option<TaskId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<DetailsView>,
    pub error: Option<ResponseError>,
    /// Total elasped time the engine was in processing state expressed as a `ISO-8601` duration format.
    #[schema(value_type = Option<String>, example = json!(null))]
    #[serde(serialize_with = "serialize_duration", default)]
    pub duration: Option<Duration>,
    /// An `RFC 3339` format for date/time/duration.
    #[schema(value_type = String, example = json!("2024-08-08_14:12:09.393Z"))]
    #[serde(with = "time::serde::rfc3339")]
    pub enqueued_at: OffsetDateTime,
    /// An `RFC 3339` format for date/time/duration.
    #[schema(value_type = String, example = json!("2024-08-08_14:12:09.393Z"))]
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub started_at: Option<OffsetDateTime>,
    /// An `RFC 3339` format for date/time/duration.
    #[schema(value_type = String, example = json!("2024-08-08_14:12:09.393Z"))]
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub finished_at: Option<OffsetDateTime>,
}

impl TaskView {
    pub fn from_task(task: &Task) -> TaskView {
        TaskView {
            uid: task.uid,
            index_uid: task.index_uid().map(ToOwned::to_owned),
            status: task.status,
            kind: task.kind.as_kind(),
            canceled_by: task.canceled_by,
            details: task.details.clone().map(DetailsView::from),
            error: task.error.clone(),
            duration: task.started_at.zip(task.finished_at).map(|(start, end)| end - start),
            enqueued_at: task.enqueued_at,
            started_at: task.started_at,
            finished_at: task.finished_at,
        }
    }
}

/// Details information of the task payload.
#[derive(Default, Debug, PartialEq, Eq, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
#[schema(rename_all = "camelCase")]
pub struct DetailsView {
    /// Number of documents received for documentAdditionOrUpdate task.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub received_documents: Option<u64>,
    /// Number of documents finally indexed for documentAdditionOrUpdate task or a documentAdditionOrUpdate batch of tasks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexed_documents: Option<Option<u64>>,
    /// Number of documents edited for editDocumentByFunction task.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edited_documents: Option<Option<u64>>,
    /// Value for the primaryKey field encountered if any for indexCreation or indexUpdate task.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_key: Option<Option<String>>,
    /// Number of provided document ids for the documentDeletion task.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provided_ids: Option<usize>,
    /// Number of documents finally deleted for documentDeletion and indexDeletion tasks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted_documents: Option<Option<u64>>,
    /// Number of tasks that match the request for taskCancelation or taskDeletion tasks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_tasks: Option<u64>,
    /// Number of tasks canceled for taskCancelation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canceled_tasks: Option<Option<u64>>,
    /// Number of tasks deleted for taskDeletion.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted_tasks: Option<Option<u64>>,
    /// Original filter query for taskCancelation or taskDeletion tasks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_filter: Option<Option<String>>,
    /// Identifier generated for the dump for dumpCreation task.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dump_uid: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<Option<Object>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<String>,
    /// [Learn more about the settings in this guide](https://www.meilisearch.com/docs/reference/api/settings).
    #[serde(skip_serializing_if = "Option::is_none")]
    // #[serde(flatten)]
    #[schema(value_type = Option<Settings<Unchecked>>)]
    pub settings: Option<Box<Settings<Unchecked>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swaps: Option<Vec<IndexSwap>>,
}

impl From<Details> for DetailsView {
    fn from(details: Details) -> Self {
        match details {
            Details::DocumentAdditionOrUpdate { received_documents, indexed_documents } => {
                DetailsView {
                    received_documents: Some(received_documents),
                    indexed_documents: Some(indexed_documents),
                    ..DetailsView::default()
                }
            }
            Details::DocumentEdition {
                deleted_documents,
                edited_documents,
                original_filter,
                context,
                function,
            } => DetailsView {
                deleted_documents: Some(deleted_documents),
                edited_documents: Some(edited_documents),
                original_filter: Some(original_filter),
                context: Some(context),
                function: Some(function),
                ..DetailsView::default()
            },
            Details::SettingsUpdate { mut settings } => {
                settings.hide_secrets();
                DetailsView { settings: Some(settings), ..DetailsView::default() }
            }
            Details::IndexInfo { primary_key } => {
                DetailsView { primary_key: Some(primary_key), ..DetailsView::default() }
            }
            Details::DocumentDeletion {
                provided_ids: received_document_ids,
                deleted_documents,
            } => DetailsView {
                provided_ids: Some(received_document_ids),
                deleted_documents: Some(deleted_documents),
                original_filter: Some(None),
                ..DetailsView::default()
            },
            Details::DocumentDeletionByFilter { original_filter, deleted_documents } => {
                DetailsView {
                    provided_ids: Some(0),
                    original_filter: Some(Some(original_filter)),
                    deleted_documents: Some(deleted_documents),
                    ..DetailsView::default()
                }
            }
            Details::ClearAll { deleted_documents } => {
                DetailsView { deleted_documents: Some(deleted_documents), ..DetailsView::default() }
            }
            Details::TaskCancelation { matched_tasks, canceled_tasks, original_filter } => {
                DetailsView {
                    matched_tasks: Some(matched_tasks),
                    canceled_tasks: Some(canceled_tasks),
                    original_filter: Some(Some(original_filter)),
                    ..DetailsView::default()
                }
            }
            Details::TaskDeletion { matched_tasks, deleted_tasks, original_filter } => {
                DetailsView {
                    matched_tasks: Some(matched_tasks),
                    deleted_tasks: Some(deleted_tasks),
                    original_filter: Some(Some(original_filter)),
                    ..DetailsView::default()
                }
            }
            Details::Dump { dump_uid } => {
                DetailsView { dump_uid: Some(dump_uid), ..DetailsView::default() }
            }
            Details::IndexSwap { swaps } => {
                DetailsView { swaps: Some(swaps), ..Default::default() }
            }
        }
    }
}

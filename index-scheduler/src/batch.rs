/*!
This module handles the creation and processing of batch operations.

A batch is a combination of multiple tasks that can be processed at once.
Executing a batch operation should always be functionally equivalent to
executing each of its tasks' operations individually and in order.

For example, if the user sends two tasks:
1. import documents X
2. import documents Y

We can combine the two tasks in a single batch:
1. import documents X and Y

Processing this batch is functionally equivalent to processing the two
tasks individally, but should be much faster since we are only performing
one indexing operation.
*/

use std::collections::HashSet;
use std::fs::File;
use std::io::BufWriter;

use dump::IndexMetadata;
use log::{debug, error, info};
use meilisearch_types::heed::{RoTxn, RwTxn};
use meilisearch_types::milli::documents::{obkv_to_object, DocumentsBatchReader};
use meilisearch_types::milli::update::{
    DocumentAdditionResult, DocumentDeletionResult, IndexDocumentsConfig, IndexDocumentsMethod,
    Settings as MilliSettings,
};
use meilisearch_types::milli::{self, BEU32};
use meilisearch_types::settings::{apply_settings_to_builder, Settings, Unchecked};
use meilisearch_types::tasks::{Details, Kind, KindWithContent, Status, Task};
use meilisearch_types::Index;
use roaring::RoaringBitmap;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::autobatcher::BatchKind;
use crate::utils::{self, swap_index_uid_in_task};
use crate::{Error, IndexScheduler, Query, Result, TaskId};

/// Represents a combination of tasks that can all be processed at the same time.
///
/// A batch contains the set of tasks that it represents (accessible through
/// [`self.ids()`](Batch::ids)), as well as additional information on how to
/// be processed.
#[derive(Debug)]
pub(crate) enum Batch {
    TaskCancelation(Task),
    TaskDeletion(Task),
    Snapshot(Vec<Task>),
    Dump(Task),
    IndexOperation { op: IndexOperation, must_create_index: bool },
    IndexCreation { index_uid: String, primary_key: Option<String>, task: Task },
    IndexUpdate { index_uid: String, primary_key: Option<String>, task: Task },
    IndexDeletion { index_uid: String, tasks: Vec<Task>, index_has_been_created: bool },
    IndexSwap { task: Task },
}

/// A [batch](Batch) that combines multiple tasks operating on an index.
#[derive(Debug)]
pub(crate) enum IndexOperation {
    DocumentImport {
        index_uid: String,
        primary_key: Option<String>,
        method: IndexDocumentsMethod,
        documents_counts: Vec<u64>,
        content_files: Vec<Uuid>,
        tasks: Vec<Task>,
    },
    DocumentDeletion {
        index_uid: String,
        documents: Vec<String>,
        tasks: Vec<Task>,
    },
    DocumentClear {
        index_uid: String,
        tasks: Vec<Task>,
    },
    Settings {
        index_uid: String,
        // TODO what's that boolean, does it mean that it removes things or what?
        settings: Vec<(bool, Settings<Unchecked>)>,
        tasks: Vec<Task>,
    },
    DocumentClearAndSetting {
        index_uid: String,
        cleared_tasks: Vec<Task>,

        // TODO what's that boolean, does it mean that it removes things or what?
        settings: Vec<(bool, Settings<Unchecked>)>,
        settings_tasks: Vec<Task>,
    },
    SettingsAndDocumentImport {
        index_uid: String,

        primary_key: Option<String>,
        method: IndexDocumentsMethod,
        documents_counts: Vec<u64>,
        content_files: Vec<Uuid>,
        document_import_tasks: Vec<Task>,

        // TODO what's that boolean, does it mean that it removes things or what?
        settings: Vec<(bool, Settings<Unchecked>)>,
        settings_tasks: Vec<Task>,
    },
}

impl Batch {
    /// Return the task ids associated with this batch.
    pub fn ids(&self) -> Vec<TaskId> {
        match self {
            Batch::TaskCancelation(task)
            | Batch::TaskDeletion(task)
            | Batch::Dump(task)
            | Batch::IndexCreation { task, .. }
            | Batch::IndexUpdate { task, .. } => vec![task.uid],
            Batch::Snapshot(tasks) | Batch::IndexDeletion { tasks, .. } => {
                tasks.iter().map(|task| task.uid).collect()
            }
            Batch::IndexOperation { op, .. } => match op {
                IndexOperation::DocumentImport { tasks, .. }
                | IndexOperation::DocumentDeletion { tasks, .. }
                | IndexOperation::Settings { tasks, .. }
                | IndexOperation::DocumentClear { tasks, .. } => {
                    tasks.iter().map(|task| task.uid).collect()
                }
                IndexOperation::SettingsAndDocumentImport {
                    document_import_tasks: tasks,
                    settings_tasks: other,
                    ..
                }
                | IndexOperation::DocumentClearAndSetting {
                    cleared_tasks: tasks,
                    settings_tasks: other,
                    ..
                } => tasks.iter().chain(other).map(|task| task.uid).collect(),
            },
            Batch::IndexSwap { task } => vec![task.uid],
        }
    }
}

impl IndexOperation {
    pub fn index_uid(&self) -> &str {
        match self {
            IndexOperation::DocumentImport { index_uid, .. }
            | IndexOperation::DocumentDeletion { index_uid, .. }
            | IndexOperation::DocumentClear { index_uid, .. }
            | IndexOperation::Settings { index_uid, .. }
            | IndexOperation::DocumentClearAndSetting { index_uid, .. }
            | IndexOperation::SettingsAndDocumentImport { index_uid, .. } => index_uid,
        }
    }
}

impl IndexScheduler {
    /// Convert an [`BatchKind`](crate::autobatcher::BatchKind) into a [`Batch`].
    ///
    /// ## Arguments
    /// - `rtxn`: read transaction
    /// - `index_uid`: name of the index affected by the operations of the autobatch
    /// - `batch`: the result of the autobatcher
    pub(crate) fn create_next_batch_index(
        &self,
        rtxn: &RoTxn,
        index_uid: String,
        batch: BatchKind,
        must_create_index: bool,
    ) -> Result<Option<Batch>> {
        match batch {
            BatchKind::DocumentClear { ids } => Ok(Some(Batch::IndexOperation {
                op: IndexOperation::DocumentClear {
                    tasks: self.get_existing_tasks(rtxn, ids)?,
                    index_uid,
                },
                must_create_index,
            })),
            BatchKind::DocumentImport { method, import_ids, .. } => {
                let tasks = self.get_existing_tasks(rtxn, import_ids)?;
                let primary_key = match &tasks[0].kind {
                    KindWithContent::DocumentAdditionOrUpdate { primary_key, .. } => {
                        primary_key.clone()
                    }
                    _ => unreachable!(),
                };

                let mut documents_counts = Vec::new();
                let mut content_files = Vec::new();
                for task in &tasks {
                    match task.kind {
                        KindWithContent::DocumentAdditionOrUpdate {
                            content_file,
                            documents_count,
                            ..
                        } => {
                            documents_counts.push(documents_count);
                            content_files.push(content_file);
                        }
                        _ => unreachable!(),
                    }
                }

                Ok(Some(Batch::IndexOperation {
                    op: IndexOperation::DocumentImport {
                        index_uid,
                        primary_key,
                        method,
                        documents_counts,
                        content_files,
                        tasks,
                    },
                    must_create_index,
                }))
            }
            BatchKind::DocumentDeletion { deletion_ids } => {
                let tasks = self.get_existing_tasks(rtxn, deletion_ids)?;

                let mut documents = Vec::new();
                for task in &tasks {
                    match task.kind {
                        KindWithContent::DocumentDeletion { ref documents_ids, .. } => {
                            documents.extend_from_slice(documents_ids)
                        }
                        _ => unreachable!(),
                    }
                }

                Ok(Some(Batch::IndexOperation {
                    op: IndexOperation::DocumentDeletion { index_uid, documents, tasks },
                    must_create_index,
                }))
            }
            BatchKind::Settings { settings_ids, .. } => {
                let tasks = self.get_existing_tasks(rtxn, settings_ids)?;

                let mut settings = Vec::new();
                for task in &tasks {
                    match task.kind {
                        KindWithContent::SettingsUpdate {
                            ref new_settings, is_deletion, ..
                        } => settings.push((is_deletion, new_settings.clone())),
                        _ => unreachable!(),
                    }
                }

                Ok(Some(Batch::IndexOperation {
                    op: IndexOperation::Settings { index_uid, settings, tasks },
                    must_create_index,
                }))
            }
            BatchKind::ClearAndSettings { other, settings_ids, allow_index_creation } => {
                let (index_uid, settings, settings_tasks) = match self
                    .create_next_batch_index(
                        rtxn,
                        index_uid,
                        BatchKind::Settings { settings_ids, allow_index_creation },
                        must_create_index,
                    )?
                    .unwrap()
                {
                    Batch::IndexOperation {
                        op: IndexOperation::Settings { index_uid, settings, tasks, .. },
                        ..
                    } => (index_uid, settings, tasks),
                    _ => unreachable!(),
                };
                let (index_uid, cleared_tasks) = match self
                    .create_next_batch_index(
                        rtxn,
                        index_uid,
                        BatchKind::DocumentClear { ids: other },
                        must_create_index,
                    )?
                    .unwrap()
                {
                    Batch::IndexOperation {
                        op: IndexOperation::DocumentClear { index_uid, tasks },
                        ..
                    } => (index_uid, tasks),
                    _ => unreachable!(),
                };

                Ok(Some(Batch::IndexOperation {
                    op: IndexOperation::DocumentClearAndSetting {
                        index_uid,
                        cleared_tasks,
                        settings,
                        settings_tasks,
                    },
                    must_create_index,
                }))
            }
            BatchKind::SettingsAndDocumentImport {
                settings_ids,
                method,
                allow_index_creation,
                import_ids,
            } => {
                let settings = self.create_next_batch_index(
                    rtxn,
                    index_uid.clone(),
                    BatchKind::Settings { settings_ids, allow_index_creation },
                    must_create_index,
                )?;

                let document_import = self.create_next_batch_index(
                    rtxn,
                    index_uid.clone(),
                    BatchKind::DocumentImport { method, allow_index_creation, import_ids },
                    must_create_index,
                )?;

                match (document_import, settings) {
                    (
                        Some(Batch::IndexOperation {
                            op:
                                IndexOperation::DocumentImport {
                                    primary_key,
                                    documents_counts,
                                    content_files,
                                    tasks: document_import_tasks,
                                    ..
                                },
                            ..
                        }),
                        Some(Batch::IndexOperation {
                            op: IndexOperation::Settings { settings, tasks: settings_tasks, .. },
                            ..
                        }),
                    ) => Ok(Some(Batch::IndexOperation {
                        op: IndexOperation::SettingsAndDocumentImport {
                            index_uid,
                            primary_key,
                            method,
                            documents_counts,
                            content_files,
                            document_import_tasks,
                            settings,
                            settings_tasks,
                        },
                        must_create_index,
                    })),
                    _ => unreachable!(),
                }
            }
            BatchKind::IndexCreation { id } => {
                let task = self.get_task(rtxn, id)?.ok_or(Error::CorruptedTaskQueue)?;
                let (index_uid, primary_key) = match &task.kind {
                    KindWithContent::IndexCreation { index_uid, primary_key } => {
                        (index_uid.clone(), primary_key.clone())
                    }
                    _ => unreachable!(),
                };
                Ok(Some(Batch::IndexCreation { index_uid, primary_key, task }))
            }
            BatchKind::IndexUpdate { id } => {
                let task = self.get_task(rtxn, id)?.ok_or(Error::CorruptedTaskQueue)?;
                let primary_key = match &task.kind {
                    KindWithContent::IndexUpdate { primary_key, .. } => primary_key.clone(),
                    _ => unreachable!(),
                };
                Ok(Some(Batch::IndexUpdate { index_uid, primary_key, task }))
            }
            BatchKind::IndexDeletion { ids } => Ok(Some(Batch::IndexDeletion {
                index_uid,
                index_has_been_created: must_create_index,
                tasks: self.get_existing_tasks(rtxn, ids)?,
            })),
            BatchKind::IndexSwap { id } => {
                let task = self.get_task(rtxn, id)?.ok_or(Error::CorruptedTaskQueue)?;
                Ok(Some(Batch::IndexSwap { task }))
            }
        }
    }

    /// Create the next batch to be processed;
    /// 1. We get the *last* task to cancel.
    /// 2. We get the *next* task to delete.
    /// 3. We get the *next* snapshot to process.
    /// 4. We get the *next* dump to process.
    /// 5. We get the *next* tasks to process for a specific index.
    pub(crate) fn create_next_batch(&self, rtxn: &RoTxn) -> Result<Option<Batch>> {
        let enqueued = &self.get_status(rtxn, Status::Enqueued)?;
        let to_cancel = self.get_kind(rtxn, Kind::TaskCancelation)? & enqueued;

        // 1. we get the last task to cancel.
        if let Some(task_id) = to_cancel.max() {
            return Ok(Some(Batch::TaskCancelation(
                self.get_task(rtxn, task_id)?.ok_or(Error::CorruptedTaskQueue)?,
            )));
        }

        // 2. we get the next task to delete
        let to_delete = self.get_kind(rtxn, Kind::TaskDeletion)? & enqueued;
        if let Some(task_id) = to_delete.min() {
            let task = self.get_task(rtxn, task_id)?.ok_or(Error::CorruptedTaskQueue)?;

            return Ok(Some(Batch::TaskDeletion(task)));
        }

        // 3. we batch the snapshot.
        let to_snapshot = self.get_kind(rtxn, Kind::Snapshot)? & enqueued;
        if !to_snapshot.is_empty() {
            return Ok(Some(Batch::Snapshot(self.get_existing_tasks(rtxn, to_snapshot)?)));
        }

        // 4. we batch the dumps.
        let to_dump = self.get_kind(rtxn, Kind::DumpExport)? & enqueued;
        if let Some(to_dump) = to_dump.min() {
            return Ok(Some(Batch::Dump(
                self.get_task(rtxn, to_dump)?.ok_or(Error::CorruptedTaskQueue)?,
            )));
        }

        // 5. We take the next task and try to batch all the tasks associated with this index.
        if let Some(task_id) = enqueued.min() {
            let task = self.get_task(rtxn, task_id)?.ok_or(Error::CorruptedTaskQueue)?;

            // This is safe because all the remaining task are associated with
            // AT LEAST one index. We can use the right or left one it doesn't
            // matter.
            let index_name = task.indexes().unwrap()[0];
            let index_already_exists = self.index_mapper.exists(rtxn, &index_name)?;

            let index_tasks = self.index_tasks(rtxn, index_name)? & enqueued;

            // If autobatching is disabled we only take one task at a time.
            let tasks_limit = if self.autobatching_enabled { usize::MAX } else { 1 };

            let enqueued = index_tasks
                .into_iter()
                .take(tasks_limit)
                .map(|task_id| {
                    self.get_task(rtxn, task_id)
                        .and_then(|task| task.ok_or(Error::CorruptedTaskQueue))
                        .map(|task| (task.uid, task.kind))
                })
                .collect::<Result<Vec<_>>>()?;

            if let Some((batchkind, create_index)) =
                crate::autobatcher::autobatch(enqueued, index_already_exists)
            {
                return self.create_next_batch_index(
                    rtxn,
                    index_name.to_string(),
                    batchkind,
                    create_index,
                );
            }
        }

        // If we found no tasks then we were notified for something that got autobatched
        // somehow and there is nothing to do.
        Ok(None)
    }

    /// Apply the operation associated with the given batch.
    ///
    /// ## Return
    /// The list of tasks that were processed. The metadata of each task in the returned
    /// list is updated accordingly, with the exception of the its date fields
    /// [`finished_at`](meilisearch_types::tasks::Task::finished_at) and [`started_at`](meilisearch_types::tasks::Task::started_at).
    pub(crate) fn process_batch(&self, batch: Batch) -> Result<Vec<Task>> {
        match batch {
            Batch::TaskCancelation(mut task) => {
                // 1. Retrieve the tasks that matched the query at enqueue-time.
                let matched_tasks =
                    if let KindWithContent::TaskCancelation { tasks, query: _ } = &task.kind {
                        tasks
                    } else {
                        unreachable!()
                    };

                let mut wtxn = self.env.write_txn()?;
                let canceled_tasks_content_uuids =
                    self.cancel_matched_tasks(&mut wtxn, task.uid, matched_tasks)?;

                task.status = Status::Succeeded;
                match &mut task.details {
                    Some(Details::TaskCancelation {
                        matched_tasks: _,
                        canceled_tasks,
                        original_query: _,
                    }) => {
                        *canceled_tasks = Some(canceled_tasks_content_uuids.len() as u64);
                    }
                    _ => unreachable!(),
                }

                // We must only remove the content files if the transaction is successfuly committed
                // and if errors occurs when we are deleting files we must do our best to delete
                // everything. We do not return the encountered errors when deleting the content
                // files as it is not a breaking operation and we can safely continue our job.
                match wtxn.commit() {
                    Ok(()) => {
                        for content_uuid in canceled_tasks_content_uuids {
                            if let Err(error) = self.delete_update_file(content_uuid) {
                                error!(
                                    "We failed deleting the content file indentified as {}: {}",
                                    content_uuid, error
                                )
                            }
                        }
                    }
                    Err(e) => return Err(e.into()),
                }

                Ok(vec![task])
            }
            Batch::TaskDeletion(mut task) => {
                // 1. Retrieve the tasks that matched the query at enqueue-time.
                let matched_tasks =
                    if let KindWithContent::TaskDeletion { tasks, query: _ } = &task.kind {
                        tasks
                    } else {
                        unreachable!()
                    };

                let mut wtxn = self.env.write_txn()?;
                let deleted_tasks_count = self.delete_matched_tasks(&mut wtxn, matched_tasks)?;

                task.status = Status::Succeeded;
                match &mut task.details {
                    Some(Details::TaskDeletion {
                        matched_tasks: _,
                        deleted_tasks,
                        original_query: _,
                    }) => {
                        *deleted_tasks = Some(deleted_tasks_count);
                    }
                    _ => unreachable!(),
                }
                wtxn.commit()?;
                Ok(vec![task])
            }
            Batch::Snapshot(_) => todo!(),
            Batch::Dump(mut task) => {
                let started_at = OffsetDateTime::now_utc();
                let (keys, instance_uid, dump_uid) = if let KindWithContent::DumpExport {
                    keys,
                    instance_uid,
                    dump_uid,
                } = &task.kind
                {
                    (keys, instance_uid, dump_uid)
                } else {
                    unreachable!();
                };
                let dump = dump::DumpWriter::new(instance_uid.clone())?;

                // 1. dump the keys
                let mut dump_keys = dump.create_keys()?;
                for key in keys {
                    dump_keys.push_key(key)?;
                }
                dump_keys.flush()?;

                let rtxn = self.env.read_txn()?;

                // 2. dump the tasks
                let mut dump_tasks = dump.create_tasks_queue()?;
                for ret in self.all_tasks.iter(&rtxn)? {
                    let (_, mut t) = ret?;
                    let status = t.status;
                    let content_file = t.content_uuid().map(|uuid| uuid.clone());

                    // In the case we're dumping ourselves we want to be marked as finished
                    // to not loop over ourselves indefinitely.
                    if t.uid == task.uid {
                        let finished_at = OffsetDateTime::now_utc();

                        // We're going to fake the date because we don't know if everything is going to go well.
                        // But we need to dump the task as finished and successful.
                        // If something fail everything will be set appropriately in the end.
                        t.status = Status::Succeeded;
                        t.started_at = Some(started_at);
                        t.finished_at = Some(finished_at);
                    }
                    let mut dump_content_file = dump_tasks.push_task(&t.into())?;

                    // 2.1. Dump the `content_file` associated with the task if there is one and the task is not finished yet.
                    if let Some(content_file) = content_file {
                        if status == Status::Enqueued {
                            let content_file = self.file_store.get_update(content_file)?;

                            let reader = DocumentsBatchReader::from_reader(content_file)
                                .map_err(milli::Error::from)?;

                            let (mut cursor, documents_batch_index) =
                                reader.into_cursor_and_fields_index();

                            while let Some(doc) =
                                cursor.next_document().map_err(milli::Error::from)?
                            {
                                dump_content_file.push_document(&obkv_to_object(
                                    &doc,
                                    &documents_batch_index,
                                )?)?;
                            }
                            dump_content_file.flush()?;
                        }
                    }
                }
                dump_tasks.flush()?;

                // 3. Dump the indexes
                for (uid, index) in self.index_mapper.indexes(&rtxn)? {
                    let rtxn = index.read_txn()?;
                    let metadata = IndexMetadata {
                        uid: uid.clone(),
                        primary_key: index.primary_key(&rtxn)?.map(String::from),
                        created_at: index.created_at(&rtxn)?,
                        updated_at: index.updated_at(&rtxn)?,
                    };
                    let mut index_dumper = dump.create_index(&uid, &metadata)?;

                    let fields_ids_map = index.fields_ids_map(&rtxn)?;
                    let all_fields: Vec<_> = fields_ids_map.iter().map(|(id, _)| id).collect();

                    // 3.1. Dump the documents
                    for ret in index.all_documents(&rtxn)? {
                        let (_id, doc) = ret?;
                        let document = milli::obkv_to_json(&all_fields, &fields_ids_map, doc)?;
                        index_dumper.push_document(&document)?;
                    }

                    // 3.2. Dump the settings
                    let settings = meilisearch_types::settings::settings(&index, &rtxn)?;
                    index_dumper.settings(&settings)?;
                }

                let path = self.dumps_path.join(format!("{}.dump", dump_uid));
                let file = File::create(path)?;
                dump.persist_to(BufWriter::new(file))?;

                // if we reached this step we can tell the scheduler we succeeded to dump ourselves.
                task.status = Status::Succeeded;
                Ok(vec![task])
            }
            Batch::IndexOperation { op, must_create_index } => {
                let index_uid = op.index_uid();
                let index = if must_create_index {
                    // create the index if it doesn't already exist
                    let mut wtxn = self.env.write_txn()?;
                    let index = self.index_mapper.create_index(&mut wtxn, index_uid)?;
                    wtxn.commit()?;
                    index
                } else {
                    let rtxn = self.env.read_txn()?;
                    self.index_mapper.index(&rtxn, index_uid)?
                };

                let mut index_wtxn = index.write_txn()?;
                let tasks = self.apply_index_operation(&mut index_wtxn, &index, op)?;
                index_wtxn.commit()?;

                Ok(tasks)
            }
            Batch::IndexCreation { index_uid, primary_key, task } => {
                let mut wtxn = self.env.write_txn()?;
                if self.index_mapper.exists(&wtxn, &index_uid)? {
                    return Err(Error::IndexAlreadyExists(index_uid));
                }
                self.index_mapper.create_index(&mut wtxn, &index_uid)?;
                wtxn.commit()?;

                self.process_batch(Batch::IndexUpdate { index_uid, primary_key, task })
            }
            Batch::IndexUpdate { index_uid, primary_key, mut task } => {
                let rtxn = self.env.read_txn()?;
                let index = self.index_mapper.index(&rtxn, &index_uid)?;

                if let Some(primary_key) = primary_key.clone() {
                    let mut index_wtxn = index.write_txn()?;
                    let mut builder = MilliSettings::new(
                        &mut index_wtxn,
                        &index,
                        self.index_mapper.indexer_config(),
                    );
                    builder.set_primary_key(primary_key);
                    let must_stop_processing = self.must_stop_processing.clone();
                    builder.execute(
                        |indexing_step| debug!("update: {:?}", indexing_step),
                        || must_stop_processing.get(),
                    )?;
                    index_wtxn.commit()?;
                }
                task.status = Status::Succeeded;
                task.details = Some(Details::IndexInfo { primary_key });

                Ok(vec![task])
            }
            Batch::IndexDeletion { index_uid, index_has_been_created, mut tasks } => {
                let wtxn = self.env.write_txn()?;

                // it's possible that the index doesn't exist
                let number_of_documents = || -> Result<u64> {
                    let index = self.index_mapper.index(&wtxn, &index_uid)?;
                    let index_rtxn = index.read_txn()?;
                    Ok(index.number_of_documents(&index_rtxn)?)
                }()
                .unwrap_or_default();

                // The write transaction is directly owned and commited inside.
                match self.index_mapper.delete_index(wtxn, &index_uid) {
                    Ok(()) => (),
                    Err(Error::IndexNotFound(_)) if index_has_been_created => (),
                    Err(e) => return Err(e),
                }

                // We set all the tasks details to the default value.
                for task in &mut tasks {
                    task.status = Status::Succeeded;
                    task.details = match &task.kind {
                        KindWithContent::IndexDeletion { .. } => {
                            Some(Details::ClearAll { deleted_documents: Some(number_of_documents) })
                        }
                        otherwise => otherwise.default_finished_details(),
                    };
                }

                Ok(tasks)
            }
            Batch::IndexSwap { mut task } => {
                let mut wtxn = self.env.write_txn()?;
                let swaps = if let KindWithContent::IndexSwap { swaps } = &task.kind {
                    swaps
                } else {
                    unreachable!()
                };
                for (lhs, rhs) in swaps {
                    self.apply_index_swap(&mut wtxn, task.uid, lhs, rhs)?;
                }
                wtxn.commit()?;
                task.status = Status::Succeeded;
                Ok(vec![task])
            }
        }
    }

    /// Swap the index `lhs` with the index `rhs`.
    fn apply_index_swap(&self, wtxn: &mut RwTxn, task_id: u32, lhs: &str, rhs: &str) -> Result<()> {
        // 1. Verify that both lhs and rhs are existing indexes
        let index_lhs_exists = self.index_mapper.index_exists(&wtxn, lhs)?;
        if !index_lhs_exists {
            return Err(Error::IndexNotFound(lhs.to_owned()));
        }
        let index_rhs_exists = self.index_mapper.index_exists(&wtxn, rhs)?;
        if !index_rhs_exists {
            return Err(Error::IndexNotFound(rhs.to_owned()));
        }

        // 2. Get the task set for index = name.
        let mut index_lhs_task_ids =
            self.get_task_ids(&Query::default().with_index(lhs.to_owned()))?;
        index_lhs_task_ids.remove_range(task_id..);
        let mut index_rhs_task_ids =
            self.get_task_ids(&Query::default().with_index(rhs.to_owned()))?;
        index_rhs_task_ids.remove_range(task_id..);

        // 3. before_name -> new_name in the task's KindWithContent
        for task_id in &index_lhs_task_ids | &index_rhs_task_ids {
            let mut task = self.get_task(&wtxn, task_id)?.ok_or(Error::CorruptedTaskQueue)?;
            swap_index_uid_in_task(&mut task, (lhs, rhs));
            self.all_tasks.put(wtxn, &BEU32::new(task_id), &task)?;
        }

        // 4. remove the task from indexuid = before_name
        // 5. add the task to indexuid = after_name
        self.update_index(wtxn, lhs, |lhs_tasks| {
            *lhs_tasks -= &index_lhs_task_ids;
            *lhs_tasks |= &index_rhs_task_ids;
        })?;
        self.update_index(wtxn, rhs, |lhs_tasks| {
            *lhs_tasks -= &index_rhs_task_ids;
            *lhs_tasks |= &index_lhs_task_ids;
        })?;

        // 6. Swap in the index mapper
        self.index_mapper.swap(wtxn, lhs, rhs)?;

        Ok(())
    }

    /// Process the index operation on the given index.
    ///
    /// ## Return
    /// The list of processed tasks.
    fn apply_index_operation<'txn, 'i>(
        &self,
        index_wtxn: &'txn mut RwTxn<'i, '_>,
        index: &'i Index,
        operation: IndexOperation,
    ) -> Result<Vec<Task>> {
        match operation {
            IndexOperation::DocumentClear { mut tasks, .. } => {
                let count = milli::update::ClearDocuments::new(index_wtxn, index).execute()?;

                let mut first_clear_found = false;
                for task in &mut tasks {
                    task.status = Status::Succeeded;
                    // The first document clear will effectively delete every documents
                    // in the database but the next ones will clear 0 documents.
                    task.details = match &task.kind {
                        KindWithContent::DocumentClear { .. } => {
                            let count = if first_clear_found { 0 } else { count };
                            first_clear_found = true;
                            Some(Details::ClearAll { deleted_documents: Some(count) })
                        }
                        otherwise => otherwise.default_details(),
                    };
                }

                Ok(tasks)
            }
            IndexOperation::DocumentImport {
                index_uid: _,
                primary_key,
                method,
                documents_counts,
                content_files,
                mut tasks,
            } => {
                let mut primary_key_has_been_set = false;
                let must_stop_processing = self.must_stop_processing.clone();
                let indexer_config = self.index_mapper.indexer_config();
                // TODO use the code from the IndexCreate operation
                if let Some(primary_key) = primary_key {
                    if index.primary_key(index_wtxn)?.is_none() {
                        let mut builder =
                            milli::update::Settings::new(index_wtxn, index, indexer_config);
                        builder.set_primary_key(primary_key);
                        builder.execute(
                            |indexing_step| debug!("update: {:?}", indexing_step),
                            || must_stop_processing.clone().get(),
                        )?;
                        primary_key_has_been_set = true;
                    }
                }

                let config = IndexDocumentsConfig { update_method: method, ..Default::default() };

                let mut builder = milli::update::IndexDocuments::new(
                    index_wtxn,
                    index,
                    indexer_config,
                    config,
                    |indexing_step| debug!("update: {:?}", indexing_step),
                    || must_stop_processing.get(),
                )?;

                let mut results = Vec::new();
                for content_uuid in content_files.into_iter() {
                    let content_file = self.file_store.get_update(content_uuid)?;
                    let reader = DocumentsBatchReader::from_reader(content_file)
                        .map_err(milli::Error::from)?;
                    let (new_builder, user_result) = builder.add_documents(reader)?;
                    builder = new_builder;

                    let user_result = match user_result {
                        Ok(count) => Ok(DocumentAdditionResult {
                            indexed_documents: count,
                            number_of_documents: count, // TODO: this is wrong, we should use the value stored in the Details.
                        }),
                        Err(e) => Err(milli::Error::from(e)),
                    };

                    results.push(user_result);
                }

                if results.iter().any(|res| res.is_ok()) {
                    let addition = builder.execute()?;
                    info!("document addition done: {:?}", addition);
                } else if primary_key_has_been_set {
                    // Everything failed but we've set a primary key.
                    // We need to remove it.
                    let mut builder =
                        milli::update::Settings::new(index_wtxn, index, indexer_config);
                    builder.reset_primary_key();
                    builder.execute(
                        |indexing_step| debug!("update: {:?}", indexing_step),
                        || must_stop_processing.clone().get(),
                    )?;
                }

                for (task, (ret, count)) in
                    tasks.iter_mut().zip(results.into_iter().zip(documents_counts))
                {
                    match ret {
                        Ok(DocumentAdditionResult { indexed_documents, number_of_documents }) => {
                            task.status = Status::Succeeded;
                            task.details = Some(Details::DocumentAdditionOrUpdate {
                                received_documents: number_of_documents,
                                indexed_documents: Some(indexed_documents),
                            });
                        }
                        Err(error) => {
                            task.status = Status::Failed;
                            task.details = Some(Details::DocumentAdditionOrUpdate {
                                received_documents: count,
                                indexed_documents: Some(count),
                            });
                            task.error = Some(error.into())
                        }
                    }
                }

                Ok(tasks)
            }
            IndexOperation::DocumentDeletion { index_uid: _, documents, mut tasks } => {
                let mut builder = milli::update::DeleteDocuments::new(index_wtxn, index)?;
                documents.iter().for_each(|id| {
                    builder.delete_external_id(id);
                });

                let DocumentDeletionResult { deleted_documents, .. } = builder.execute()?;

                for (task, documents) in tasks.iter_mut().zip(documents) {
                    task.status = Status::Succeeded;
                    task.details = Some(Details::DocumentDeletion {
                        received_document_ids: documents.len(),
                        deleted_documents: Some(deleted_documents),
                    });
                }

                Ok(tasks)
            }
            IndexOperation::Settings { index_uid: _, settings, mut tasks } => {
                let indexer_config = self.index_mapper.indexer_config();
                // TODO merge the settings to only do *one* reindexation.
                for (task, (_, settings)) in tasks.iter_mut().zip(settings) {
                    let checked_settings = settings.clone().check();
                    task.details = Some(Details::SettingsUpdate { settings });

                    let mut builder =
                        milli::update::Settings::new(index_wtxn, index, indexer_config);
                    apply_settings_to_builder(&checked_settings, &mut builder);
                    let must_stop_processing = self.must_stop_processing.clone();
                    builder.execute(
                        |indexing_step| debug!("update: {:?}", indexing_step),
                        || must_stop_processing.get(),
                    )?;

                    task.status = Status::Succeeded;
                }

                Ok(tasks)
            }
            IndexOperation::SettingsAndDocumentImport {
                index_uid,
                primary_key,
                method,
                documents_counts,
                content_files,
                document_import_tasks,
                settings,
                settings_tasks,
            } => {
                let settings_tasks = self.apply_index_operation(
                    index_wtxn,
                    index,
                    IndexOperation::Settings {
                        index_uid: index_uid.clone(),
                        settings,
                        tasks: settings_tasks,
                    },
                )?;

                let mut import_tasks = self.apply_index_operation(
                    index_wtxn,
                    index,
                    IndexOperation::DocumentImport {
                        index_uid,
                        primary_key,
                        method,
                        documents_counts,
                        content_files,
                        tasks: document_import_tasks,
                    },
                )?;

                let mut tasks = settings_tasks;
                tasks.append(&mut import_tasks);
                Ok(tasks)
            }
            IndexOperation::DocumentClearAndSetting {
                index_uid,
                cleared_tasks,
                settings,
                settings_tasks,
            } => {
                let mut import_tasks = self.apply_index_operation(
                    index_wtxn,
                    index,
                    IndexOperation::DocumentClear {
                        index_uid: index_uid.clone(),
                        tasks: cleared_tasks,
                    },
                )?;

                let settings_tasks = self.apply_index_operation(
                    index_wtxn,
                    index,
                    IndexOperation::Settings { index_uid, settings, tasks: settings_tasks },
                )?;

                let mut tasks = settings_tasks;
                tasks.append(&mut import_tasks);
                Ok(tasks)
            }
        }
    }

    /// Delete each given task from all the databases (if it is deleteable).
    ///
    /// Return the number of tasks that were actually deleted.
    fn delete_matched_tasks(&self, wtxn: &mut RwTxn, matched_tasks: &RoaringBitmap) -> Result<u64> {
        // 1. Remove from this list the tasks that we are not allowed to delete
        let enqueued_tasks = self.get_status(wtxn, Status::Enqueued)?;
        let processing_tasks = &self.processing_tasks.read().unwrap().processing.clone();

        let all_task_ids = self.all_task_ids(&wtxn)?;
        let mut to_delete_tasks = all_task_ids & matched_tasks;
        to_delete_tasks -= processing_tasks;
        to_delete_tasks -= enqueued_tasks;

        // 2. We now have a list of tasks to delete, delete them

        let mut affected_indexes = HashSet::new();
        let mut affected_statuses = HashSet::new();
        let mut affected_kinds = HashSet::new();

        for task_id in to_delete_tasks.iter() {
            let task = self.get_task(wtxn, task_id)?.ok_or(Error::CorruptedTaskQueue)?;
            if let Some(task_indexes) = task.indexes() {
                affected_indexes.extend(task_indexes.into_iter().map(|x| x.to_owned()));
            }
            affected_statuses.insert(task.status);
            affected_kinds.insert(task.kind.as_kind());
            // Note: don't delete the persisted task data since
            // we can only delete succeeded, failed, and canceled tasks.
            // In each of those cases, the persisted data is supposed to
            // have been deleted already.
            utils::remove_task_datetime(wtxn, self.enqueued_at, task.enqueued_at, task.uid)?;
            if let Some(started_at) = task.started_at {
                utils::remove_task_datetime(wtxn, self.started_at, started_at, task.uid)?;
            }
            if let Some(finished_at) = task.finished_at {
                utils::remove_task_datetime(wtxn, self.finished_at, finished_at, task.uid)?;
            }
        }

        for index in affected_indexes {
            self.update_index(wtxn, &index, |bitmap| *bitmap -= &to_delete_tasks)?;
        }

        for status in affected_statuses {
            self.update_status(wtxn, status, |bitmap| *bitmap -= &to_delete_tasks)?;
        }

        for kind in affected_kinds {
            self.update_kind(wtxn, kind, |bitmap| *bitmap -= &to_delete_tasks)?;
        }

        for task in to_delete_tasks.iter() {
            self.all_tasks.delete(wtxn, &BEU32::new(task))?;
        }

        Ok(to_delete_tasks.len())
    }

    /// Cancel each given task from all the databases (if it is cancelable).
    ///
    /// Returns the content files that the transaction owner must delete if the commit is successful.
    fn cancel_matched_tasks(
        &self,
        wtxn: &mut RwTxn,
        cancel_task_id: TaskId,
        matched_tasks: &RoaringBitmap,
    ) -> Result<Vec<Uuid>> {
        let now = OffsetDateTime::now_utc();

        // 1. Remove from this list the tasks that we are not allowed to cancel
        //    Notice that only the _enqueued_ ones are cancelable and we should
        //    have already aborted the indexation of the _processing_ ones
        let cancelable_tasks = self.get_status(wtxn, Status::Enqueued)?;
        let tasks_to_cancel = cancelable_tasks & matched_tasks;

        // 2. We now have a list of tasks to cancel, cancel them
        let mut content_files_to_delete = Vec::new();
        for mut task in self.get_existing_tasks(wtxn, tasks_to_cancel.iter())? {
            if let Some(uuid) = task.content_uuid() {
                content_files_to_delete.push(*uuid);
            }
            task.status = Status::Canceled;
            task.canceled_by = Some(cancel_task_id);
            task.finished_at = Some(now);
            self.update_task(wtxn, &task)?;
        }

        Ok(content_files_to_delete)
    }
}

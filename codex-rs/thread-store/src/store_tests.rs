use super::*;
use crate::ReadThreadInferenceIdentitySidecarParams;
use crate::ThreadInferenceIdentitySidecarPatch;

struct DefaultStore;

macro_rules! required {
    ($($name:ident: $arg:ty => $output:ty),+ $(,)?) => {$(
        fn $name(&self, _: $arg) -> ThreadStoreFuture<'_, $output> { unreachable!() }
    )+};
}

impl ThreadStore for DefaultStore {
    fn as_any(&self) -> &dyn Any {
        self
    }
    required! {
        create_thread: CreateThreadParams => (), resume_thread: ResumeThreadParams => (),
        append_items: AppendThreadItemsParams => (), persist_thread: ThreadId => (),
        flush_thread: ThreadId => (), shutdown_thread: ThreadId => (),
        discard_thread: ThreadId => (), load_history: LoadThreadHistoryParams => StoredThreadHistory,
        read_thread: ReadThreadParams => StoredThread,
        read_thread_by_rollout_path: ReadThreadByRolloutPathParams => StoredThread,
        list_threads: ListThreadsParams => ThreadPage,
        update_thread_metadata: UpdateThreadMetadataParams => StoredThread,
        archive_thread: ArchiveThreadParams => (), unarchive_thread: ArchiveThreadParams => StoredThread,
        delete_thread: DeleteThreadParams => (),
    }
}

#[tokio::test]
async fn inference_identity_default_is_object_safe_empty_noop_and_stably_unsupported() {
    let store: &dyn ThreadStore = &DefaultStore;
    let Err(ThreadStoreError::Unsupported { operation }) = store
        .read_thread_inference_identity_sidecar(ReadThreadInferenceIdentitySidecarParams {
            thread_id: ThreadId::default(),
            include_archived: true,
        })
        .await
    else {
        panic!("default read should be unsupported");
    };
    assert_eq!(operation, "read_thread_inference_identity_sidecar");

    let mut params = UpdateThreadInferenceIdentitySidecarParams {
        thread_id: ThreadId::default(),
        patch: ThreadInferenceIdentitySidecarPatch::default(),
    };
    store
        .update_thread_inference_identity_sidecar(params.clone())
        .await
        .expect("empty no-op");
    params.patch.configured = Some(None);
    let Err(ThreadStoreError::Unsupported { operation }) =
        store.update_thread_inference_identity_sidecar(params).await
    else {
        panic!("non-empty default should be unsupported");
    };
    assert_eq!(operation, "update_thread_inference_identity_sidecar");

    let latest_request_only = UpdateThreadInferenceIdentitySidecarParams {
        thread_id: ThreadId::default(),
        patch: ThreadInferenceIdentitySidecarPatch {
            configured: None,
            latest_request: Some(None),
        },
    };
    assert!(matches!(
        store
            .update_thread_inference_identity_sidecar(latest_request_only)
            .await,
        Err(ThreadStoreError::Unsupported {
            operation: "update_thread_inference_identity_sidecar"
        })
    ));
}

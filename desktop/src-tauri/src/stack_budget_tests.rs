//! Stack budget for futures that Tauri moves on the Windows main thread.
//!
//! The Windows executable's main thread runs IPC command dispatch. In release
//! builds tauri 2.11 `InvokeResolver::respond_async_serialized` passes the
//! command future by value into `async_runtime::spawn`, which only boxes it
//! deeper inside tokio, so the future is copied onto that thread's stack more
//! than once first. The larger tokio worker stacks installed in `run()` do not
//! cover this copy. A command future that embeds the mesh-llm start chain by
//! value was ~540 KB in the 0.5.21 release binary and overflowed the 1 MiB
//! main-thread stack (`0xc00000fd` in `__chkstk`).
//!
//! Each check names the future's type from the function item without calling
//! it, so no app, state or runtime is needed.
use std::future::Future;

use crate::app_state::AppState;
use crate::mesh_llm::{DesktopMeshRuntime, StartMeshNodeRequest};
use tauri::{AppHandle, State};

const MAIN_THREAD_FUTURE_BUDGET: usize = 16 * 1024;

fn returned_future_size<Args, Fut, F>(_function: F) -> usize
where
    F: CallWith<Args, Output = Fut>,
    Fut: Future,
{
    std::mem::size_of::<Fut>()
}

trait CallWith<Args> {
    type Output;
}

impl<F, A, Fut> CallWith<(A,)> for F
where
    F: FnOnce(A) -> Fut,
{
    type Output = Fut;
}

impl<F, A, B, Fut> CallWith<(A, B)> for F
where
    F: FnOnce(A, B) -> Fut,
{
    type Output = Fut;
}

impl<F, A, B, C, Fut> CallWith<(A, B, C)> for F
where
    F: FnOnce(A, B, C) -> Fut,
{
    type Output = Fut;
}

fn assert_within_budget(name: &str, size: usize) {
    eprintln!("stack-budget: {name} future is {size} bytes (budget {MAIN_THREAD_FUTURE_BUDGET})");
    assert!(
        size <= MAIN_THREAD_FUTURE_BUDGET,
        "{name} future is {size} bytes; Tauri copies command futures onto the \
         1 MiB Windows main thread, so keep it under {MAIN_THREAD_FUTURE_BUDGET} \
         bytes (Box::pin the heavy await)"
    );
}

#[test]
fn mesh_runtime_start_future_fits_main_thread_budget() {
    let size = returned_future_size::<(StartMeshNodeRequest,), _, _>(DesktopMeshRuntime::start);
    assert_within_budget("DesktopMeshRuntime::start", size);
}

#[test]
fn constructed_mesh_runtime_start_future_fits_main_thread_budget() {
    // Built but never polled: nothing starts, only the future object exists.
    let future = DesktopMeshRuntime::start(StartMeshNodeRequest {
        mode: crate::mesh_llm::MeshNodeMode::Client,
        model_id: None,
        max_vram_gb: None,
        join_token: None,
        mesh_name: None,
        relay_url: None,
        trusted_owner_ids: None,
    });
    assert_within_budget("constructed DesktopMeshRuntime::start", std::mem::size_of_val(&future));
}

#[test]
fn mesh_start_node_command_future_fits_main_thread_budget() {
    let size = returned_future_size::<
        (AppHandle, State<'static, AppState>, StartMeshNodeRequest),
        _,
        _,
    >(crate::commands::mesh_start_node);
    assert_within_budget("mesh_start_node", size);
}

#[test]
fn start_managed_agent_command_future_fits_main_thread_budget() {
    let size = returned_future_size::<(String, AppHandle, State<'static, AppState>), _, _>(
        crate::commands::start_managed_agent,
    );
    assert_within_budget("start_managed_agent", size);
}

#[test]
fn ensure_relay_mesh_for_record_future_fits_main_thread_budget() {
    let size = returned_future_size::<(&'static AppHandle, Option<&'static str>, bool), _, _>(
        crate::commands::ensure_relay_mesh_for_record,
    );
    assert_within_budget("ensure_relay_mesh_for_record", size);
}

#[test]
fn restore_mesh_sharing_future_fits_main_thread_budget() {
    let size = returned_future_size::<(&'static AppHandle, &'static AppState), _, _>(
        crate::commands::mesh_llm::restore_mesh_sharing,
    );
    assert_within_budget("restore_mesh_sharing", size);
}

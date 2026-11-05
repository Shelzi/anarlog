use cidre::{core_audio as ca, os};

use super::DEVICE_IS_RUNNING_SOMEWHERE;
use super::state::SharedContext;

pub(super) struct ListenerData {
    pub(super) ctx: SharedContext,
    pub(super) device_listener_ptr: *mut (),
}

pub(super) fn is_mic_running(device: &ca::Device) -> Option<bool> {
    device
        .prop::<u32>(&DEVICE_IS_RUNNING_SOMEWHERE)
        .map(|v| v != 0)
        .ok()
}

/// The mic is in use when ANY input device is running somewhere, not only the
/// default one. A device that runs forever (Krisp, Loopback) keeps this true;
/// per-app events then still come from `poll_apps`.
///
/// An empty set is idle. `None` means devices exist but none could be read, so
/// the caller falls back to polling.
fn aggregate_running(states: impl IntoIterator<Item = Option<bool>>) -> Option<bool> {
    let mut any_device = false;
    let mut any_known = false;
    for state in states {
        any_device = true;
        match state {
            Some(true) => return Some(true),
            Some(false) => any_known = true,
            None => {}
        }
    }
    (!any_device || any_known).then_some(false)
}

/// The devices that must carry a listener, given every listed device and
/// whether it has input streams (`None`: that read failed). Watched: the default
/// input device, devices with input streams, and registered devices whose read
/// failed (one transient error must not unregister them). Unlisted devices drop.
///
/// A device that gains input streams without a device-list change (for example
/// Bluetooth A2DP -> HFP) is not watched until the next resync. Per-app polling
/// still covers it while any watched device is in use.
fn watch_set<T: Copy + PartialEq>(
    registered: &[T],
    listed: &[(T, Option<bool>)],
    default: Option<T>,
) -> Vec<T> {
    listed
        .iter()
        .filter(|(device, has_input)| {
            Some(*device) == default
                || match has_input {
                    Some(has_input) => *has_input,
                    None => registered.contains(device),
                }
        })
        .map(|(device, _)| *device)
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum RemovalOutcome {
    /// The listener is gone and CoreAudio cannot call it any more.
    Released,
    /// The device no longer exists, so the removal error is expected. A late
    /// notification for it may still be queued: the context must not be freed.
    Gone,
    /// CoreAudio may still hold the callback context: it must be retained.
    Orphaned,
    /// Removal failed; keep the device registered so a later sync or the final
    /// cleanup retries (and retains the context if that fails too).
    Retry,
}

/// Classifies a failed listener removal.
fn removal_outcome(is_bad_obj: bool, device_still_listed: bool) -> RemovalOutcome {
    if !device_still_listed {
        // Unplugging can give BAD_OBJ or BAD_DEVICE; any error means "gone".
        RemovalOutcome::Gone
    } else if is_bad_obj {
        RemovalOutcome::Orphaned
    } else {
        RemovalOutcome::Retry
    }
}

fn list_devices_with_input() -> os::Result<Vec<(ca::Device, Option<bool>)>> {
    Ok(ca::System::devices()?
        .into_iter()
        .map(|device| {
            let has_input = device
                .prop_size(&ca::PropSelector::DEVICE_STREAMS.input_addr())
                .ok()
                .map(|size| size > 0);
            (device, has_input)
        })
        .collect())
}

/// Running state across every input device that exists right now.
pub(super) fn any_input_device_running() -> Option<bool> {
    let listed = list_devices_with_input().ok()?;
    let default = ca::System::default_input_device().ok();
    aggregate_running(watch_set(&[], &listed, default).iter().map(is_mic_running))
}

/// Running state across the registered devices, or across all existing input
/// devices while none is registered.
pub(super) fn registered_devices_running(ctx: &SharedContext) -> Option<bool> {
    let devices = ctx.input_devices_snapshot();
    if devices.is_empty() {
        return any_input_device_running();
    }
    aggregate_running(devices.iter().map(is_mic_running))
}

fn device_still_listed(device: &ca::Device) -> bool {
    // If listing fails, assume it exists: the safe (retaining) answer.
    ca::System::devices().map_or(true, |devices| devices.contains(device))
}

/// Removes the running-state listener from `device` and classifies the result.
pub(super) fn remove_device_listener(
    device: &ca::Device,
    device_listener_ptr: *mut (),
) -> RemovalOutcome {
    match device.remove_prop_listener(
        &DEVICE_IS_RUNNING_SOMEWHERE,
        device_listener,
        device_listener_ptr,
    ) {
        Ok(()) => RemovalOutcome::Released,
        Err(error) => {
            let outcome = removal_outcome(
                error == ca::hardware_err::BAD_OBJ,
                device_still_listed(device),
            );
            match outcome {
                RemovalOutcome::Gone => tracing::debug!("input_device_already_removed"),
                _ => tracing::error!(
                    ?error,
                    tags.error.code = error.status().0,
                    ?outcome,
                    "removing_device_listener_failed"
                ),
            }
            outcome
        }
    }
}

/// Registers the running-state listener on every input device and removes it
/// from devices that went away. Safe to call repeatedly.
///
/// `sync_lock` serializes syncs and `Drop`. `input_devices` is not held across
/// CoreAudio calls (a removal waits for in-flight callbacks); the new set is
/// committed under a brief lock. Callers refresh the mic state afterwards.
pub(super) fn sync_input_devices(ctx: &SharedContext, device_listener_ptr: *mut ()) {
    let _sync = ctx.lock_sync();
    // `Drop` deactivates callbacks before taking `sync_lock`, so no listener is
    // registered once teardown has started.
    if !ctx.listener_callbacks_active() {
        return;
    }

    let listed = match list_devices_with_input() {
        Ok(listed) => listed,
        Err(error) => {
            tracing::warn!(
                ?error,
                tags.error.code = error.status().0,
                "listing_input_devices_failed"
            );
            return;
        }
    };

    let registered = ctx.input_devices_snapshot();
    let default = ca::System::default_input_device().ok();
    let wanted = watch_set(&registered, &listed, default);
    let removed = registered.iter().filter(|device| !wanted.contains(device));
    let added = wanted.iter().filter(|device| !registered.contains(device));

    let mut forgotten = Vec::new();
    for &device in removed {
        match remove_device_listener(&device, device_listener_ptr) {
            RemovalOutcome::Retry => continue,
            RemovalOutcome::Gone => ctx.leak_device_listener_context(),
            RemovalOutcome::Orphaned => ctx.require_device_listener_context_retention(),
            RemovalOutcome::Released => {}
        }
        forgotten.push(device);
    }

    let mut registered_now = Vec::new();
    for &device in added {
        match device.add_prop_listener(
            &DEVICE_IS_RUNNING_SOMEWHERE,
            device_listener,
            device_listener_ptr,
        ) {
            Ok(()) => registered_now.push(device),
            Err(error) => {
                // This device is not observed, so poll as a safety net.
                ctx.require_polling_fallback();
                tracing::error!(
                    ?error,
                    tags.error.code = error.status().0,
                    "adding_device_listener_failed"
                );
            }
        }
    }

    let mut devices = ctx
        .input_devices
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    devices.retain(|device| !forgotten.contains(device));
    devices.extend(registered_now);
}

/// Re-reads the running state of every registered input device and applies it.
pub(super) fn refresh_mic_state(ctx: &SharedContext) {
    let _serialized = ctx.lock_refresh();

    match registered_devices_running(ctx) {
        Some(running) => {
            ctx.disable_polling_fallback();
            ctx.handle_mic_change(running);
        }
        None => {
            ctx.enable_polling_fallback();
            tracing::warn!("no_input_device_state_available");
        }
    }
}

/// Initial state read at registration. Holds `refresh_lock` so a callback result
/// that arrives meanwhile cannot be overwritten by this older read.
pub(super) fn seed_mic_state(ctx: &SharedContext) {
    let _serialized = ctx.lock_refresh();

    match registered_devices_running(ctx) {
        Some(mic_in_use) => ctx.seed_running_state(mic_in_use),
        None => {
            ctx.enable_polling_fallback();
            tracing::warn!("no_input_device_state_available");
        }
    }
}

pub(super) extern "C-unwind" fn device_listener(
    _obj_id: ca::Obj,
    number_addresses: u32,
    addresses: *const ca::PropAddr,
    client_data: *mut (),
) -> os::Status {
    let data = unsafe { &*(client_data as *const ListenerData) };
    if !data.ctx.listener_callbacks_active() {
        return os::Status::NO_ERR;
    }
    let addresses = unsafe { std::slice::from_raw_parts(addresses, number_addresses as usize) };

    // The callback may come from a device that was just removed, so the state is
    // always recomputed from the whole registered set instead of this one device.
    if addresses
        .iter()
        .any(|addr| addr.selector == ca::PropSelector::DEVICE_IS_RUNNING_SOMEWHERE)
    {
        refresh_mic_state(&data.ctx);
    }

    os::Status::NO_ERR
}

pub(super) extern "C-unwind" fn system_listener(
    _obj_id: ca::Obj,
    number_addresses: u32,
    addresses: *const ca::PropAddr,
    client_data: *mut (),
) -> os::Status {
    let data = unsafe { &*(client_data as *const ListenerData) };
    if !data.ctx.listener_callbacks_active() {
        return os::Status::NO_ERR;
    }
    let addresses = unsafe { std::slice::from_raw_parts(addresses, number_addresses as usize) };

    // Devices added or removed, or the default input changed (the new default can
    // be a device we have not seen yet). Both are handled by one resync.
    if addresses.iter().any(|addr| {
        addr.selector == ca::PropSelector::HW_DEVICES
            || addr.selector == ca::PropSelector::HW_DEFAULT_INPUT_DEVICE
    }) {
        sync_input_devices(&data.ctx, data.device_listener_ptr);
        refresh_mic_state(&data.ctx);
    }

    os::Status::NO_ERR
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_running_device_means_mic_in_use() {
        assert_eq!(
            aggregate_running([Some(false), Some(true), Some(false)]),
            Some(true)
        );
        // An unreadable device does not hide a running one.
        assert_eq!(aggregate_running([None, Some(true)]), Some(true));
    }

    #[test]
    fn idle_unknown_and_empty_device_sets() {
        assert_eq!(aggregate_running([Some(false), Some(false)]), Some(false));
        assert_eq!(aggregate_running([None, Some(false)]), Some(false));
        assert_eq!(aggregate_running([None, None]), None);
        assert_eq!(aggregate_running([]), Some(false));
    }

    #[test]
    fn watch_set_follows_input_streams_and_the_device_list() {
        let listed = [(1, Some(true)), (2, Some(false)), (3, Some(true))];
        assert_eq!(watch_set(&[], &listed, None), vec![1, 3]);
        // Unplugged devices (1, 3) are dropped.
        assert_eq!(watch_set(&[1, 2, 3], &[(2, Some(true))], None), vec![2]);
    }

    #[test]
    fn watch_set_keeps_registered_device_on_transient_read_error() {
        let listed = [(1, None), (2, None)];
        // 1 is registered: keep it. 2 is unknown: do not add it yet.
        assert_eq!(watch_set(&[1], &listed, None), vec![1]);
    }

    #[test]
    fn watch_set_always_includes_default_input_device() {
        let listed = [(1, Some(false)), (2, Some(true))];
        assert_eq!(watch_set(&[], &listed, Some(1)), vec![1, 2]);
        // A default device that CoreAudio does not list cannot be watched.
        assert_eq!(watch_set(&[], &listed, Some(9)), vec![2]);
    }

    #[test]
    fn removal_error_on_an_unlisted_device_means_gone() {
        for is_bad_obj in [true, false] {
            assert_eq!(removal_outcome(is_bad_obj, false), RemovalOutcome::Gone);
        }
    }

    #[test]
    fn removal_error_on_a_listed_device_keeps_the_old_behaviour() {
        assert_eq!(removal_outcome(true, true), RemovalOutcome::Orphaned);
        assert_eq!(removal_outcome(false, true), RemovalOutcome::Retry);
    }
}

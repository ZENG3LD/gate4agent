# node-core cut: server.rs / NodeShared inventory

Status: branch `migrate/node-core-extract`. Heuristic classification (name + body keyword density); "mixed" methods touch both transport state and product catalogs and need a port trait before they can move.

## Already in gate4agent-node-core

`platform`, `spawn_spec`, `kit_wireguard` (feature `wireguard`), `materialization`, `managed_worktree`, `session_registry`, `drive_loop_profile`.

## Summary of `impl NodeShared` (server.rs, lines 3393-14581)

- transport: 102 methods, ~3560 lines
- mixed: 16 methods, ~4209 lines
- product: 53 methods, ~3418 lines

## NodeShared fields

| field group | fields | class |
|---|---|---|
| identity/auth | `access_token`, `node_id`, `incarnation_id`, `started_at_unix_ms`, `preauth_slots`, `authenticated_slots`, `inspection_slots` | transport |
| runtime/handle | `handle`, `native_session_gauge`, `input_settle_timeout_ms`, `shutdown`, `shutdown_notify`, `mutation_gate` | transport |
| events | `event_tx`, `terminal_event_tx`, `agent_stream_event_tx`, `history`, `terminal_frame_watermarks`, `next_*` ids | transport |
| sessions/registry | `session_bindings`, `session_records`, `spawn_idempotency`, `managed_spawn_replays`, `controller`, `state_path`, `persistence_error`, `state_transaction`, `session_record_retention`, `retired_records_total` | transport (registry already in node-core) |
| profiling/metrics | `drive_loop_profile`, `runtime_tick_profile`, `shell_efficiency_profile`, `connection_*`, `rejected_*`, `drive_loop_iterations_*` | transport |
| providers | `enabled_providers`, `provider_runtime_*`, `provider_contracts`, `provider_adapter_contracts`, `spawn_profiles`, `native_launch_profile_control`, `native_session_catalog` | transport (provider launch port) |
| workspaces | `workspaces` | transport root registry; file browsing is product |
| worktree | `worktree_service_modes`, `managed_worktree_profiles`, `managed_worktrees` | product |
| environment | `environment_profiles`, `environment_materialization_profiles`, `session_environment_materializer`, `materializations`, `network_allowlist_catalog` | product |
| bundle/context | `bundle_catalog`, `delivery_store`, `context_pack_store`, `context_catalog` | product |
| harness/browser | `harness_mcp_registry`, `harness_mcp_proxy_event_tx`, `browser_station_leases` (feature dig2-station-probe) | product |

## Proposed port traits (node-core)

- `SpawnPreparer`: given a spawn request, returns env/overlay/args/cleanup guard (wraps worktree, environment, bundle, context-pack, harness overlays).
- `SessionLifecycleHook`: on session start/exit/retire (release worktree lease, materialization, harness registration).
- `ProductRoutes`: extra HTTP/command handlers (file browser, git, bundle, context pack, station) registered by the node binary.
- `StateExtension`: product sections of the durable state (worktree leases, materializations, delivery replays) read/written beside the registry.

## Methods (line, length, class, product-keyword hits in body)

| line | len | method | class | hits |
|---|---|---|---|---|
| 3394 | 156 | `new_with_incarnation` | mixed | 55 |
| 3550 | 32 | `new` | transport | 0 |
| 3582 | 7 | `persistence_error` | transport | 0 |
| 3589 | 69 | `admit_provider_runtime` | transport | 0 |
| 3658 | 12 | `provider_supports_pipe_transport` | transport | 0 |
| 3670 | 22 | `provider_supports_acp_transport` | transport | 0 |
| 3692 | 39 | `admit_declared_hook_semantics` | mixed | 10 |
| 3731 | 97 | `resolve_network_allowlist_overlay_args` | product | 68 |
| 3828 | 16 | `resolve_codex_network_access_overlay_args` | product | 11 |
| 3844 | 95 | `resolve_environment_profile` | product | 67 |
| 3939 | 26 | `resolve_bundle` | product | 14 |
| 3965 | 22 | `begin_delivery_stage` | product | 8 |
| 3987 | 27 | `put_delivery_blob_chunk` | product | 9 |
| 4014 | 33 | `commit_delivery_stage` | product | 13 |
| 4047 | 17 | `abort_delivery_stage` | product | 6 |
| 4064 | 21 | `resolve_context` | transport | 0 |
| 4085 | 43 | `bundle_layout` | product | 16 |
| 4128 | 58 | `select_environment_profile` | product | 7 |
| 4186 | 36 | `install_prepared_launch_overlay` | product | 18 |
| 4222 | 11 | `environment_materialization_profile` | product | 5 |
| 4233 | 104 | `materialization_profile` | product | 41 |
| 4337 | 233 | `prepare_session_materialization` | product | 83 |
| 4570 | 76 | `cleanup_materialization` | product | 14 |
| 4646 | 202 | `resolve_record_materialization` | product | 89 |
| 4848 | 125 | `destroy_record_materialization` | product | 13 |
| 4973 | 16 | `mark_materialization_recovery_required` | product | 5 |
| 4989 | 180 | `reconcile_materializations` | product | 61 |
| 5169 | 34 | `replay_spawn_spec` | transport | 0 |
| 5203 | 20 | `remember_spawn_spec` | transport | 0 |
| 5223 | 29 | `claim_harness_mcp_spawn` | product | 4 |
| 5252 | 32 | `replay_managed_spawn` | transport | 2 |
| 5284 | 18 | `remember_managed_spawn` | transport | 2 |
| 5302 | 12 | `remember_managed_spawn_attempt` | transport | 2 |
| 5314 | 99 | `acquire_durable_managed_spawn_v2` | mixed | 14 |
| 5413 | 33 | `link_durable_managed_spawn_v2_lease_locked` | transport | 7 |
| 5446 | 11 | `restore_durable_managed_spawn_v2_locked` | transport | 1 |
| 5457 | 43 | `commit_durable_managed_spawn_v2` | mixed | 8 |
| 5500 | 35 | `remove_unallocated_durable_managed_spawn_v2` | transport | 2 |
| 5535 | 32 | `resolve_spawn_spec` | transport | 6 |
| 5567 | 35 | `resolve_spawn_workspace` | mixed | 17 |
| 5602 | 7 | `spawn_from_spec` | transport | 0 |
| 5609 | 78 | `spawn_from_spec_with_proxy` | mixed | 23 |
| 5687 | 80 | `spawn_from_spec_with_harness_mcp` | product | 13 |
| 5767 | 7 | `spawn_managed_worktree` | product | 4 |
| 5774 | 360 | `spawn_managed_worktree_inner` | product | 59 |
| 6134 | 71 | `spawn_managed_worktree_v2` | product | 15 |
| 6205 | 22 | `managed_profile` | transport | 7 |
| 6227 | 43 | `reconcile_failed_allocation` | mixed | 12 |
| 6270 | 141 | `reconcile_managed_worktrees` | product | 30 |
| 6411 | 47 | `reconcile_context_pack_exports` | product | 3 |
| 6458 | 35 | `reconcile_context_pack_export_for_record` | product | 8 |
| 6493 | 42 | `set_exported_context` | transport | 0 |
| 6535 | 20 | `managed_record_export_target_for_instance` | transport | 0 |
| 6555 | 18 | `mark_managed_recovery_required` | transport | 5 |
| 6573 | 8 | `set_persistence_error` | transport | 0 |
| 6581 | 8 | `persist_state` | transport | 0 |
| 6589 | 51 | `persist_state_locked` | transport | 5 |
| 6640 | 5 | `begin_shutdown` | transport | 0 |
| 6645 | 42 | `begin_shutdown_locked` | transport | 2 |
| 6687 | 21 | `ensure_binding_capacity` | transport | 2 |
| 6708 | 24 | `ensure_workspace_environment_profile_compatible` | product | 5 |
| 6732 | 17 | `bind_session` | transport | 0 |
| 6749 | 16 | `bind_session_with_policy` | transport | 3 |
| 6765 | 30 | `bind_session_with_materialization` | product | 10 |
| 6795 | 18 | `bind_managed_session` | transport | 3 |
| 6813 | 34 | `bind_managed_session_with_materialization` | product | 10 |
| 6847 | 22 | `bind_spawn_session` | transport | 3 |
| 6869 | 102 | `bind_spawn_session_with_materialization` | product | 30 |
| 6971 | 31 | `allocate_record_id` | transport | 0 |
| 7002 | 28 | `allocate_task_id` | transport | 0 |
| 7030 | 14 | `default_record_name` | transport | 0 |
| 7044 | 35 | `new_record` | transport | 5 |
| 7079 | 42 | `insert_record` | transport | 0 |
| 7121 | 15 | `record` | transport | 0 |
| 7136 | 24 | `revalidate_session_record_preview` | transport | 0 |
| 7160 | 44 | `revalidate_session_record_context_export` | transport | 0 |
| 7204 | 19 | `revalidate_loaded_history` | transport | 0 |
| 7223 | 8 | `remove_record_memory` | transport | 0 |
| 7231 | 25 | `discard_record` | transport | 1 |
| 7256 | 4 | `publish_record` | transport | 0 |
| 7260 | 37 | `mark_record_error` | transport | 0 |
| 7297 | 40 | `rename_session_record` | transport | 0 |
| 7337 | 83 | `set_session_task` | transport | 0 |
| 7420 | 16 | `index_provider_session` | transport | 0 |
| 7436 | 88 | `index_provider_session_with_policy` | transport | 2 |
| 7524 | 16 | `bound_address_for_record` | transport | 0 |
| 7540 | 36 | `forget_session_record` | transport | 2 |
| 7576 | 53 | `remove_managed_session_record` | transport | 7 |
| 7629 | 105 | `retire_unavailable_session_records` | transport | 0 |
| 7734 | 268 | `resume_session_record` | mixed | 46 |
| 8002 | 54 | `wait_for_record_resume` | transport | 0 |
| 8056 | 9 | `workspace_root` | transport | 0 |
| 8065 | 9 | `workspace_roots` | transport | 0 |
| 8074 | 97 | `inspect_workspace` | mixed | 14 |
| 8171 | 36 | `browse_host_directories` | mixed | 10 |
| 8207 | 71 | `read_workspace_file` | product | 6 |
| 8278 | 40 | `write_workspace_file` | product | 4 |
| 8318 | 40 | `create_workspace_file` | product | 2 |
| 8358 | 34 | `create_workspace_directory` | product | 5 |
| 8392 | 14 | `read_git_history` | product | 2 |
| 8406 | 12 | `read_git_diff` | product | 2 |
| 8418 | 17 | `native_session_route_root` | transport | 0 |
| 8435 | 60 | `project_native_session_entries` | transport | 0 |
| 8495 | 51 | `catalog_native_sessions` | transport | 0 |
| 8546 | 66 | `page_native_sessions` | transport | 0 |
| 8612 | 51 | `preview_native_session` | transport | 0 |
| 8663 | 69 | `index_native_session` | transport | 0 |
| 8732 | 39 | `preview_native_session_by_session_id` | transport | 0 |
| 8771 | 125 | `register_workspace` | transport | 4 |
| 8896 | 64 | `create_standalone_workspace` | transport | 1 |
| 8960 | 81 | `unregister_workspace` | transport | 0 |
| 9041 | 63 | `create_worktree` | product | 20 |
| 9104 | 63 | `remove_worktree` | product | 20 |
| 9167 | 160 | `cleanup_managed_worktree` | product | 58 |
| 9327 | 18 | `mark_managed_cleanup_blocked` | transport | 5 |
| 9345 | 28 | `require_worktree_service` | product | 10 |
| 9373 | 16 | `managed_reservation_conflict` | transport | 2 |
| 9389 | 15 | `reject_managed_reservation` | transport | 2 |
| 9404 | 38 | `bound_workspace_root` | transport | 0 |
| 9442 | 43 | `require_session_runtime_policy` | transport | 0 |
| 9485 | 50 | `remove_binding` | mixed | 14 |
| 9535 | 33 | `cleanup_session_owned_materialization` | product | 7 |
| 9568 | 14 | `clear_terminal_frame_watermark` | transport | 0 |
| 9582 | 16 | `address_for` | transport | 0 |
| 9598 | 26 | `arm_resume` | transport | 0 |
| 9624 | 14 | `clear_armed_resume` | transport | 0 |
| 9638 | 198 | `publish_control` | transport | 4 |
| 9836 | 395 | `reconcile_managed_record` | mixed | 31 |
| 10231 | 137 | `snapshot` | mixed | 23 |
| 10368 | 65 | `publish` | mixed | 8 |
| 10433 | 28 | `publish_transient` | transport | 6 |
| 10461 | 31 | `publish_terminal_frames` | transport | 0 |
| 10492 | 80 | `publish_terminal_frame_candidates` | transport | 0 |
| 10572 | 31 | `publish_agent_stream_chunk` | transport | 2 |
| 10603 | 7 | `current_sequence` | transport | 0 |
| 10610 | 14 | `controller_state` | transport | 0 |
| 10624 | 62 | `acquire_controller` | transport | 0 |
| 10686 | 18 | `release_controller` | transport | 0 |
| 10704 | 16 | `require_controller` | transport | 0 |
| 10720 | 53 | `validate_address` | transport | 0 |
| 10773 | 4 | `dispatch` | transport | 0 |
| 10777 | 8 | `prepare_command` | transport | 0 |
| 10785 | 19 | `dispatch_envelope` | transport | 0 |
| 10804 | 31 | `record_rejected_command` | transport | 0 |
| 10835 | 10 | `take_rejected_command_reason` | transport | 0 |
| 10845 | 17 | `dispatch_bounded` | transport | 0 |
| 10862 | 29 | `internal_session_snapshot` | transport | 0 |
| 10891 | 14 | `require_acp_session` | transport | 0 |
| 10905 | 91 | `dispatch_history_bounded` | transport | 0 |
| 10996 | 31 | `discover_history` | transport | 1 |
| 11027 | 41 | `load_history` | transport | 0 |
| 11068 | 25 | `export_context_pack_for_session_record` | product | 2 |
| 11093 | 97 | `export_context_pack_for_session_record_inner` | product | 5 |
| 11190 | 8 | `export_context_pack` | product | 4 |
| 11198 | 64 | `materialize_context_pack` | product | 7 |
| 11262 | 16 | `commit_context_pack_durably` | product | 3 |
| 11278 | 17 | `commit_context_pack` | product | 3 |
| 11295 | 97 | `commit_context_pack_for_session_record` | product | 3 |
| 11392 | 13 | `context_pack_repository` | product | 4 |
| 11405 | 83 | `context_pack_repository_observation` | product | 7 |
| 11488 | 52 | `forget_context_pack` | product | 4 |
| 11540 | 82 | `dispatch_input_bounded` | transport | 0 |
| 11622 | 86 | `dispatch_resize_bounded` | transport | 0 |
| 11708 | 47 | `spawn_session` | transport | 4 |
| 11755 | 574 | `spawn_session_with_deadline` | mixed | 178 |
| 12329 | 14 | `effective_spawn_runtime_policy` | transport | 0 |
| 12343 | 11 | `verify_harness_mcp_before_start` | product | 4 |
| 12354 | 19 | `rollback_spawn` | transport | 1 |
| 12373 | 80 | `remove_session` | transport | 6 |
| 12453 | 34 | `wait_until_removed` | transport | 0 |
| 12487 | 2094 | `resync` | mixed | 299 |

## Port status

- `SpawnPreparer`: `prepare` (spawn block), `select_environment`, `install_launch_overlay` (also used by `resume_session_record`). Not routed: `prepare_session_materialization` (called only from `prepare`), `spawn_managed_worktree*`/`reconcile_*` (worktree lease logic, product-only; no launch preparation of their own).
- `SessionLifecycleHook`: `on_binding_removed`, `on_session_ended`, `on_shutdown`, `on_harness_reservation_aborted`, `cleanup_session_materialization`.
- `ProductRoutes`, `StateExtension`: not started.
- Traits still use product types (`SessionBinding`, `PreparedNativeLaunchOverlay`, `NodeFailure`, receipts, guards). They cannot move to node-core until neutral types replace them, and `NodeShared` itself (one 11k-line impl, ~100 product fields/methods) cannot cross the crate boundary before ProductRoutes/StateExtension exist.
- The legacy `mod tests` in server.rs is `cfg(all(test, windows))`; Linux coverage lives in `spawn_characterization_tests` and sibling modules.

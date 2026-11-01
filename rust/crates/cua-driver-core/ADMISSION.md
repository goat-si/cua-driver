# Desktop-input admission inventory

The table covers the union of core tools and the production macOS, Linux and
Windows registrations. Some tools are platform- or configuration-dependent.
"Yes" means the file lease and coordinator are required, except when the
implementation attests an exact-window background independent input lane.
An unknown operation on a mixed tool is treated as a mutation.

| Tool | Admitted? | Reason |
| --- | --- | --- |
| `click`, `double_click`, `right_click`, `drag`, `scroll` | Yes | Pointer input changes application or desktop state. |
| `mouse_button_down`, `mouse_button_up`, `mouse_drag`, `parallel_mouse_drag` | Yes | Pointer input changes application or desktop state. |
| `move_cursor`, `type_text`, `press_key`, `hotkey`, `set_value`, `select_text` | Yes | Cursor, keyboard, value or selection mutation. |
| `bring_to_front`, `set_window_frame`, `invoke_menu` | Yes | Focus, window geometry or menu action mutation. |
| `launch_app`, `kill_app`, `clipboard_write` | Yes | Process or desktop clipboard mutation. |
| `menu_bar_extras` | `system`, `list`: no; other operations: yes | Discovery and enumeration read; `open`, `click`, `close` change menu state. |
| `page` | `get_text`, `query_dom`: no; other actions: yes | Only the two named actions are reads; JavaScript can mutate. |
| `browser_dialog` | `inspect`: no; other actions: yes | Inspection reads; accept and dismiss change browser state. |
| `browser_prepare`, `browser_navigate`, `browser_click`, `browser_type`, `browser_select` | Yes | Browser launch, navigation or page input. |
| `browser_set_input_files`, `browser_download`, `browser_pointer` | Yes | Changes page input, triggers a download or sends pointer input. |
| `list_apps`, `list_windows`, `get_window_state`, `get_desktop_state`, `get_accessibility_tree` | No | Read-only observation. |
| `get_screen_size`, `get_cursor_position`, `get_browser_state`, `clipboard_read`, `zoom` | No | Read-only observation. |
| `verify_state`, `parse_visual_regions`, `debug_window_info`, `health_report`, `get_guide` | No | Read-only observation or diagnostics. |
| `get_config`, `get_agent_cursor_state`, `get_recording_state` | No | Read-only driver state. |
| `get_session`, `get_session_state`, `list_sessions`, `history_status`, `history_query` | No | Read-only session or history state. |
| `start_session`, `end_session`, `escalate_session` | No | Session lifecycle or capture policy; cleanup must not wait on its own retained lease. |
| `start_recording`, `stop_recording`, `install_ffmpeg` | No | Recording control or recording dependency installation. |
| `set_config`, `set_agent_cursor_enabled`, `set_agent_cursor_motion`, `set_agent_cursor_theme` | No | Driver configuration or agent overlay settings. |
| `install_extension` | No | Installs a driver extension; does not touch application or desktop state. Tools an extension provides are admitted by the non-read-only default. |
| `run_steps`, `run_script`, `replay_trajectory` | No | Composite dispatcher; each nested registry call is admitted independently. |
| `check_permissions` | macOS `prompt=true`: yes; otherwise no | A trusted prompt can show a system dialog; passive checks read status. Linux and Windows declare the tool read-only. |

`type_text_chars` is an invoke-time alias of `type_text` on production
platforms. Linux's window-close operation is dispatched by `hotkey`, not a
separate registered tool. Non-native build stubs may have different metadata
and do not implement desktop actions.

`run_steps`, `run_script`, and `replay_trajectory` are the production
registry re-entry sites. They do not hold the outer coordinator; nested calls
cross the same admission boundary. The core composite-dispatch test exercises
both free and held lease cases.

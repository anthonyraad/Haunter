# Vendored egui-baseview

This is egui-baseview at rev `ec70c3fe6b2f070dcacbc22924431edbe24bd1c0`, the revision nih-plug pins, with two changes in `src/window.rs`:

- baseview's file drag events (`DragEntered`, `DragMoved`, `DragLeft`, `DragDropped`) are passed to egui as `hovered_files` and `dropped_files`, and file drops are accepted with a copy effect. Upstream ignores them, so Windows shows a "not allowed" cursor and the files never reach the plugin.
- On Windows, the plugin window takes keyboard focus when an egui text field gains focus, and gives it back to the host's parent window when the field loses it. Windows only sends keys to the focused window, which in a host is the host's own window, so without this typing in a text field does nothing. Handing focus back afterwards lets the host's shortcuts, like the space bar, work again. This replaces upstream's `windows_keyboard_workaround`, which grabs focus on every widget event and keeps it. It needs `winapi` (`winuser`, `windef`), which baseview already uses.

Haunter's `Cargo.toml` swaps it in with `[patch."https://github.com/BillyDM/egui-baseview.git"]`.

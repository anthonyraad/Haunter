# Vendored egui-baseview

This is egui-baseview at rev `ec70c3fe6b2f070dcacbc22924431edbe24bd1c0`, the revision nih-plug pins, with one change in `src/window.rs`: baseview's file drag events (`DragEntered`, `DragMoved`, `DragLeft`, `DragDropped`) are passed to egui as `hovered_files` and `dropped_files`, and file drops are accepted with a copy effect. Upstream ignores them, so Windows shows a "not allowed" cursor and the files never reach the plugin.

Ghostpump's `Cargo.toml` swaps it in with `[patch."https://github.com/BillyDM/egui-baseview.git"]`.

# Editor LSP

`crates/app/src/lsp.rs` is a minimal stdio LSP client; the editor panels use it
for diagnostics (red/amber ranges + gutter bars + `✕n ⚠n` in the pane header).

## How it works

- One background thread (`lsp::spawn`) owns every language-server process, so
  the UI thread never blocks on a server.
- Servers start lazily on the first file that maps to them, with `rootUri` =
  nearest ancestor holding `Cargo.toml`, `package.json`, `tsconfig.json`,
  `pyproject.toml`, `setup.py`, `go.mod`, `requirements.txt`, `.git`, `.hg`,
  `Makefile`, `build.zig` or `flake.nix`.
- Documents are synced with full text: `didOpen` once, then `didChange`
  (skipped above 4 MB), `didClose` when the editor panel closes.
- `textDocument/publishDiagnostics` is translated to byte ranges (UTF-16 →
  UTF-8) and applied as `RangeDecoration`s (severity fill) plus
  `GutterDecoration`s.

## Supported languages (auto-detected by extension)

| extensions | server | install |
|---|---|---|
| `rs` | `rust-analyzer` | `rustup component add rust-analyzer` |
| `ts tsx js jsx mjs cjs` | `typescript-language-server --stdio` | `npm i -g typescript typescript-language-server` |
| `py pyi` | `basedpyright-langserver` → `pyright-langserver` → `pylsp` | `npm i -g basedpyright` or `pip install python-lsp-server` |
| `go` | `gopls` | `go install golang.org/x/tools/gopls@latest` |
| `c h cc cpp cxx hpp hh` | `clangd` | distro package `clangd` |
| `lua` | `lua-language-server` | distro package |
| `toml` | `taplo lsp stdio` | `cargo install taplo-cli --features lsp` |
| `yaml yml` | `yaml-language-server --stdio` | `npm i -g yaml-language-server` |
| `json jsonc` | `vscode-json-language-server --stdio` | `npm i -g vscode-langservers-extracted` |
| `sh bash zsh` | `bash-language-server start` | `npm i -g bash-language-server` |
| `md markdown` | `marksman server` | `cargo install marksman` / release binary |
| `zig` | `zls` | release binary |
| `nix` | `nixd` | nixpkgs |

A missing binary is skipped silently (one log line), the editor still works.

## Overriding a server

Any extension can be pinned with `OMD_LSP_<EXT>` (whitespace-separated command):

```sh
OMD_LSP_PY="/usr/bin/python3 /tmp/my-server.py --stdio" omd
OMD_LSP_RS="$HOME/.cargo/bin/rust-analyzer --log-file /tmp/ra.log" omd
```

## Editor features

`lsp::Providers` implements gpui-base's `CompletionProvider`, `HoverProvider`
and `DefinitionProvider` and is installed through `EditorState::lsp_mut()` when
an editor is opened for a file that has a server.

| feature | trigger |
|---|---|
| diagnostics | automatic (`publishDiagnostics`) |
| completion | typing (auto-trigger on word chars / `.` `:` `/` `"`) and `Ctrl+Space` |
| hover | mouse over a symbol |
| go to definition | `F12` |
| code actions (quick fix / 自动 import) | `Ctrl+.` |
| 语义高亮 | 自动（文档变化后 debounce 100ms 拉 `semanticTokens/full`） |

Keybindings are registered in `main()` inside the `Input` context, because
gpui-base defines the actions but no default keys.

Byte offset ↔ LSP position conversion is `lsp::position_for` (UTF-16 columns)
and `lsp::offset_for`.

Code actions apply the returned `WorkspaceEdit` through
`EditorState::apply_lsp_edits` (sorted by descending start offset so earlier
inserts do not shift later ranges), then run any `Command` through
`workspace/executeCommand`.

Edits are grouped **by file**:

- the file the action came from → applied to that editor (undo/caret intact);
- another file that is open in a panel → queued in `pending_workspace_edits`
  and applied on the next render (only a render has `&mut Window`);
- a file that is not open → read with `std::fs`, transformed by
  `lsp::apply_text_edits` (UTF-16 positions → byte offsets, descending order),
  written back.

The provider hands foreign files back to the UI through
`Event::ApplyWorkspaceEdit`, because only the UI knows which buffers are open.

Saving a dirty editor (`Ctrl+S`) writes the file and sends
`textDocument/didSave`.

### Auto-import (completion `additionalTextEdits`)

gpui-base inserts the accepted item's text but ignores `additionalTextEdits`,
so the provider caches `(insert text -> edits)` for the last completion
response (`Providers::extras`). The app subscribes to `InputEditsEvent`: when
the inserted text matches a cached entry, the extra edits are queued and applied
on the next render (`EditorState::apply_lsp_edits`, sorted by descending start
offset). The probe `OMD_LSP_ACCEPT=1` exercises the whole path.

### Semantic highlighting

`initialize` captures `capabilities.semanticTokensProvider.legend` and stores it
per path in a process-wide registry (`legend_for`), because
`DocumentRangeSemanticTokensProvider::legend()` is synchronous. The provider
requests `textDocument/semanticTokens/full` (the whole document; gpui-base
caches and filters to the viewport at paint time) and lsp-types decodes the
delta-encoded `data` array. Token *type names* are resolved against the active
`HighlightTheme` at paint time, so `keyword` / `function` / `variable` / … get
the same colors as the syntect path.

The first fetch happens through `EditorState::refresh()`, which sets the
editor's pending-update flag — without it a freshly opened document would stay
uncolored until the first keystroke.

## Smoke hooks

- `OMD_LSP_COMPLETE=1`: after the first diagnostics arrive, dispatch
  `ShowCompletions` at the first LSP-backed editor and log the item count.
- `OMD_LSP_ACTION=1`: after the first diagnostics arrive, query
  `textDocument/codeAction` for the first LSP-backed editor and log the count.
- `OMD_LSP_SEMANTIC=1`: after the first diagnostics arrive, fetch semantic
  tokens once and log how many came back.
- `OMD_LSP_ACCEPT=1`: query completions, then insert the fake server's label so
  the `additionalTextEdits` path runs end to end.
- `OMD_LSP_APPLY=1`: query code actions and perform the first one — the fake
  server returns an edit that also touches `other.rs`, so this exercises the
  cross-file path.
- `OMD_TYPE_TEST=1`: insert text into the first editor and log the length plus
  the `didChange` the server receives.
- `OMD_LSP_<EXT>`: pin the server command for an extension (see above).

Server stderr is forwarded to the app log as `[lsp:<server>] …`; request and
parse results are logged as `[lsp] completion: N items` / `hover: hit|none` /
`definitions: N links`.

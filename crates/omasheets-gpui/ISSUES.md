# omasheets-gpui behavior tests

Command for every section:

```
cargo test -p omasheets-gpui --test spreadsheet_ui -- --test-threads=1
```

Result: 2 passed, 4 failed.

## Passed

- `painted_fill_matches_projected_appearance` — generated workbook (explicit RGB fill, merge, custom column width) and, because the file was present, one explicit-fill cell of `spreadsheetbench_verified__spreadsheet__1_53647__input.xlsx`. Painted quad color and size matched `project_xlsx_appearance`.
- `independent_package_read_agrees_with_appearance_projection` — stored-zip read of the generated package agreed with the projection on merge count, RGB `FFC41E3A`, and custom width 20.

## Cells are not clickable in a headless frame

Test: `clicking_a_cell_and_entering_a_formula_commits_and_displays_the_result`

Assertion: click `cell-0-0`, type `=1+2`, press Enter. The host receives `EditCommitted` for sheet `Sheet`, A1, source `=1+2`, and the cell then displays `3`.

Observed: panic in `gpui-kit` `test.rs` — `missing ElementId Name("cell-0-0")`. Registered paths included `Name("omasheets-spreadsheet")` only as an ancestor of the formula `Input` (`NamedInteger("input", 4294967299)`). No `cell-*` id was registered. The commit and display assertions did not run.

Product gap: grid cells are plain divs with ids, but they do not opt into test observation or expose the painted text as an accessibility value, so a host cannot click a cell or see that it shows 3.

## Selection cannot be moved by clicking another cell

Test: `clicking_another_cell_moves_the_selection_and_formula_bar`

Assertion: after committing `=1+2` on A1, click `cell-0-1`. The host receives `SelectionChanged` for B1, and the formula bar shows that cell (empty) instead of `=1+2`.

Observed: the same panic as the previous test, on `window.click("cell-0-0")`, before B1 was clicked. `SelectionChanged` and the formula-bar assertion did not run.

Product gap: the same missing cell observation. A headless host cannot change the selection by clicking, so it also cannot check that the formula bar follows the new cell.

## Rejected-command notification was read before it could arrive

Test: `rejected_command_emits_command_failed_without_changing_the_document`

Assertion: `apply_command(AddSheet { name: "Sheet" })` emits `CommandFailed` whose message contains `DuplicateName`, and the document digest is unchanged.

Observed: panic `a duplicate sheet name should emit CommandFailed, got []`. The digest assertion did not run.

Product gap: none was shown. `Context::emit` only queues `Effect::Emit`, and this assertion read the host list inside `update_window`, before GPUI flushes that queue. An empty list here does not mean the view failed to emit.

## No-selection formula commit was read before it could arrive

Test: `committing_a_formula_with_no_selection_emits_command_failed`

Assertion: after `DeleteSheet` clears the selection, typing `=1+2` into the formula bar and pressing Enter emits `CommandFailed` containing `no cell is selected`, does not emit `EditCommitted`, and leaves the document digest unchanged.

Observed: the formula bar was found and Enter was pressed, then panic `enter in the formula bar with no selection should emit CommandFailed, got []`. The `EditCommitted` and digest assertions did not run.

Product gap: none was shown. The event list was read in the same window update that would have queued the emit, so this run did not show whether a formula commit with no selection notifies the host or changes the document.

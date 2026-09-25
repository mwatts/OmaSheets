# Spreadsheet-RL sample

Source: [Spreadsheet-RL](https://huggingface.co/datasets/Spreadsheet-RL/Spreadsheet-RL) on Hugging Face, released with the Spreadsheet-RL paper and code drop dated 2026-05-17. License: CC-BY-SA-4.0.

The recorded baseline is a 16-file sample of workbooks from that release (Excel-forum and SpreadsheetBench tasks). Three further SpreadsheetBench-2 workbooks were added to the local sample on 2026-09-24 and scored on their own; that pass is below and is not folded into the 16-file table. Workbooks are not vendored. This sample is not the Enron corpus and not the full 33,015-file zip. The full zip was not scored.

## Results

[`spreadsheet-rl.score-summary.json`](spreadsheet-rl.score-summary.json) is the aggregate two-lane score of that 16-file sample. Rescored 2026-09-25 at `405867e`, after the occupied-cell budget. The formula totals are unchanged from the `0f169a8` score. The record is aggregate only: no workbook paths, cell contents, or per-file results.

The owned lane opened every workbook. The Formualizer candidate lane opened 11 of 16, so its formula-cell totals cover only those 11. The five candidate opens that failed stay in the file count (2 formula-parse errors, 1 undefined name, 2 undefined tables). None timed out.

| Owned engine lane | Spreadsheet-RL sample (this pass) |
|---|---:|
| Workbooks opened | 16 / 16 |
| Formula cells observed | 14,391 |
| Loaded and compared | 14,386 (99.97%) |
| Stored values matched | 12,558 |
| Match rate of compared | 87.29% |
| Stored values mismatched | 1,828 |
| Not compiled | 5 |

### Refusals

Of the 5 formula cells the owned engine did not compile, every one is a cycle. `GETPIVOTDATA` compiles. Structured references from the earlier measurement (628) still compile. No sheet was skipped, and the owned lane rejected no workbook for the 1904 date system or the two-million-cell limit.

Stored-value mismatches are a separate count from refusals. All 1,828 disagreements are an empty stored cache against a calculated value: blank against a number (1,690), blank against text (119), blank against `#DIV/0!` (15), and blank against `#VALUE!` (4). Kind labels are not cell contents.

The candidate lane observed 5,264 formula cells in the 11 workbooks it opened and parsed all of them. It also observed 18,204 value cells in those workbooks. That formula total is not comparable with the owned lane's 14,391, because five workbooks never opened on the candidate lane.

These figures describe one 16-file sample scored once. They are not the frozen Enron 1,000-workbook score and not a measurement of the 33,015-file zip.

The score ran on Darwin arm64. The probe address-space limit cannot be applied on this host (`RLIMIT_AS` returns `EINVAL`), so that call was allowed to succeed and no engine source was changed for the score itself. The occupied-cell rescore reproduced the formula totals. Lane resident-set peaks reported by the scorer on that run were 70,287,360 bytes (candidate) and 84,934,656 bytes (owned).

## Added complex workbooks

Three SpreadsheetBench-2 inputs with a reputation for structural complexity were copied into the local sample and scored the same day, on the same uncommitted tree, with a 120-second timeout. They are not in the 16-file totals above. Near-duplicate siblings (the other Debugging 10_* operating rollups, and the other Financial_Model 04_* and 09_* valuation files) were not added. The candidate lane opened all three and parsed every formula it saw.

| Workbook | Sheets | Formulas | Compared | Matched | Mismatched | Not compiled |
|---|---:|---:|---:|---:|---:|---:|
| Debugging 10_01, 99-sheet operating rollup | 99 | 77,194 | 77,151 | 77,151 | 0 | 43 cycles |
| Financial_Model 08_04, DCF and three statements | 13 | 133,888 | 133,881 | 133,881 | 0 | 7 cycles |
| Financial_Model 04_02, valuation model | 9 | 74,578 | 74,578 | 74,572 | 6 | 0 |
| Three-file total at `405867e` |  | 285,660 | 285,610 | 285,604 | 6 | 50 |

The first score of these three, before the OFFSET wait, matched 84.10% of compared cells (45,399 mismatched, almost all in the DCF workbook). The table above was first recorded at `0f169a8` and reproduced the same day at `405867e`, with a 120-second timeout. The operating rollup matches every formula that compiled. Its 43 refusals are scenario holds that read their own cell on the inactive branch, such as `IF($C$2=3,$R31,AV20)` in `AV20`. The valuation model's six mismatches are `((later/earlier)^(1/5)-1)` on a negative ratio: the cache is a real fifth root and the engine returns `#NUM!`. Its seven `PROPER` formulas match, so that workbook has nothing left uncompiled. The DCF workbook has no stored-value mismatches, and the same seven cycles.

A dynamic `OFFSET` discovered during the calculation pass now waits for the formula cells in the rectangle it resolves. `PMT` for an integer number of periods is evaluated at 16 significant digits, so `-PMT(0.06/12,84,50000000)` matches the cached payment instead of running about 1.29e-8 high. Reimporting the DCF workbook after both changes leaves no stored-value mismatches. The 7 cycles are an annual total that sums the months, where each month is a fraction of that total.

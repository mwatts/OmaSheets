# Spreadsheet-RL sample

Source: [Spreadsheet-RL](https://huggingface.co/datasets/Spreadsheet-RL/Spreadsheet-RL) on Hugging Face, released with the Spreadsheet-RL paper and code drop dated 2026-05-17. License: CC-BY-SA-4.0.

The recorded baseline is a 16-file sample of workbooks from that release (Excel-forum and SpreadsheetBench tasks). Three further SpreadsheetBench-2 workbooks were added to the local sample on 2026-09-24 and scored on their own; that pass is below and is not folded into the 16-file table. Workbooks are not vendored. This sample is not the Enron corpus and not the full 33,015-file zip. The full zip was not scored.

## Results

[`spreadsheet-rl.score-summary.json`](spreadsheet-rl.score-summary.json) is the aggregate two-lane score of that 16-file sample. Scored 2026-09-24 on the uncommitted tree after `f08a50a`. The record is aggregate only: no workbook paths, cell contents, or per-file results.

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

The score ran on Darwin arm64. The probe address-space limit cannot be applied on this host (`RLIMIT_AS` returns `EINVAL`), so that call was allowed to succeed and no engine source was changed for the score itself. A rescore of this same tree after the later Enron calculation fixes left the formula totals unchanged. Lane resident-set peaks reported by the scorer were 69,615,616 bytes (candidate) and 85,016,576 bytes (owned).

## Added complex workbooks

Three SpreadsheetBench-2 inputs with a reputation for structural complexity were copied into the local sample and scored the same day, on the same uncommitted tree, with a 120-second timeout. They are not in the 16-file totals above. Near-duplicate siblings (the other Debugging 10_* operating rollups, and the other Financial_Model 04_* and 09_* valuation files) were not added. The candidate lane opened all three and parsed every formula it saw.

| Workbook | Sheets | Formulas | Compared | Matched | Mismatched | Not compiled |
|---|---:|---:|---:|---:|---:|---:|
| Debugging 10_01, 99-sheet operating rollup | 99 | 77,194 | 77,151 | 77,151 | 0 | 43 cycles |
| Financial_Model 08_04, DCF and three statements | 13 | 133,888 | 133,881 | 133,876 | 5 | 7 cycles |
| Financial_Model 04_02, valuation model | 9 | 74,578 | 74,571 | 74,565 | 6 | 7 `PROPER` |
| Three-file total after the `OFFSET` wait |  | 285,660 | 285,603 | 285,592 | 11 | 57 |

The first score of these three, before that wait, matched 84.10% of compared cells (45,399 mismatched, almost all in the DCF workbook). Owned peak resident set was 148,094,976 bytes. Candidate peak was 504,332,288 bytes. The DCF row above is a later reimport on the same tree. The other two rows are from the first score, and the total adds those rows to the reimport.

The operating rollup matches every formula that compiled. Its 43 refusals are scenario holds that read their own cell on the inactive branch, such as `IF($C$2=3,$R31,AV20)` in `AV20`. The valuation model's six mismatches are `((later/earlier)^(1/5)-1)` on a negative ratio: the cache is a real fifth root and the engine returns `#NUM!`. Its seven refusals are `PROPER`.

A dynamic `OFFSET` discovered during the calculation pass now waits for the formula cells in the rectangle it resolves. Reimporting the DCF workbook after that change leaves 5 number mismatches and the same 7 cycles. The cycles are an annual total that sums the months, where each month is a fraction of that total. The 5 mismatches are the funding-schedule close. `Capex and Debt Assumptions!D29` is `-PMT(0.06/12,84,50000000)`, and that payment is high by 1.29e-8. From Funding Schedule row 22 the draw uses it, and the excess compounds to about 1.3e-6 by row 105, so `IF(close<=0,0,close)` drops the last draw. Rows 16 through 21, the interest-only months, match.

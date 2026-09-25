# Supported formula functions

The owned M0 engine (`crates/omasheets-calc`) accepts exactly the
129 function names listed below, grouped for reading.
A test in the calc crate fails when this file and the registry disagree, so
the count here is never edited by hand: add the function to the registry and
regenerate this list.

Operators: `+ - * / ^ & %`, unary `+`/`-`, comparisons `= <> < <= > >=`,
error literals (`#REF!`, `#N/A`, `#DIV/0!`, `#VALUE!`, `#NUM!`, `#NAME?`,
`#NULL!`, and `Sheet!#REF!` for a deleted cell on another sheet), omitted arguments,
rectangular ranges of any size inside the Excel grid, including whole columns and rows such as `A:A`, `A:XFD`, `1:1` and `1:1048576` (including qualified endpoints and deleted endpoints
such as `A1:#REF!`, which evaluate to `#REF!`), absolute markers,
cross-sheet references, workbook and sheet-scoped defined names (including
`Sheet!LocalName`; tokens past
the grid such as `Table1` are names), implicit intersection of a range in
scalar position, and elementwise evaluation of range expressions inside
aggregate arguments (`SUM(IF(A1:A5=0,0,B1:B5))`, `SUMPRODUCT((A1:A5>2)*B1:B5)`).
Rectangular array constants support numbers, text, booleans and error literals,
comma-separated columns and semicolon-separated rows, up to 1,000,000 values.
They work in aggregates, elementwise expressions and INDEX/MATCH/LOOKUP,
VLOOKUP/HLOOKUP/XLOOKUP. A scalar use takes the first value. A root
`TRANSPOSE`, `MMULT` or array constant spills into the rectangle whose
top-left is the formula, up to 1,000,000 values; a blocked or out-of-grid
rectangle is `#SPILL!`. `_xlfn.` and `_xlfn._xlws.` prefixes
resolve only to functions already in the registry.

`INDEX` also returns references: `SUM(A1:INDEX(A1:A100,D1))` follows the
selector in D1, and a zero row or column selects that entire axis. The range
operator binds a bounded envelope of all possible endpoint selections. Native
replay preserves that envelope's row and column identities, including after
sorting. Constant row/column selections narrow the calculation dependencies after
stable binding, so unused source columns do not create false cycles. Dynamic axes
retain their bounded envelope; potential cycles within it are still refused. A moved formula
whose current A1 spelling cannot preserve those identities reports a projection
refusal instead of exporting different references.

`TODAY`, `NOW`, `RAND` and `RANDBETWEEN` read the stored tick and never the
system clock. With no tick they are `#N/A`. Import sets that tick from the
cached numeric value of a `TODAY()` or `NOW()` cell, as a 1900 serial read
in UTC, before formulas are installed. `NOW()` keeps the time fraction when
the cache has one. A workbook with no such cached cell stays at no tick.
`OFFSET` with constant arguments
is an ordinary range; a dynamic shift keeps that shift's rectangle as its
dependency envelope. `INDIRECT` accepts one A1 reference or range, optionally
sheet-qualified. An external workbook reference (`[1]Sheet!A1`,
`[Book.xlsx]Sheet!A1`, or `'[Book.xlsx]Sheet 1'!A1`) compiles and reads the
target workbook when that file is a relative path next to the source. The
stored external-link cache is used when the file is absent. A same-named
file reached only as the base name of an absolute or `file://` target does
not replace a populated cache. A cell on a sheet that cache or target
workbook knows, with no stored value, is blank, and a formula that returns
that blank shows 0. A sheet whose refresh failed still returns the cells
the cache lists; a cell that sheet does not list is `#REF!`. An unknown
sheet or link is `#REF!`. A missing cell inside a cached external range
on a sheet that refreshed is blank.
Deliberately unsupported: 3D references, add-in (`_xll.`) calls, and the
1904 date system. VBA and other workbook-defined procedures are not
Excel functions and are not implemented. `TEXT` accepts only the locale-free codes listed
with the text functions below.

Approximate lookups (`VLOOKUP`/`HLOOKUP` without `FALSE`, `MATCH` types 1 and
-1) binary-search sorted keys per Excel's documented contract; results over
unsorted keys are undefined in Excel and are not promised here.

## Registry

### Matrices and databases

- `TRANSPOSE`
- `MMULT`
- `LINEST`
- `DAVERAGE`
- `DMAX`
- `DMIN`
- `DSTDEV`

`TRANSPOSE` and `MMULT` produce bounded arrays consumed by aggregates and
lookups; scalar uses take the first element. MMULT requires numeric, nonblank
inputs with matching inner dimensions, at most 1,000,000 output values and
50,000,000 multiply-add terms. Larger products return `#NUM!`.

Database aggregates resolve fields by heading or one-based column number.
Criteria columns on one row are ANDed; rows are ORed. Duplicate headings,
blank criteria, text prefixes, wildcards and comparison operators are supported.
At most 50,000,000 candidate/criterion comparisons are allowed per call.
Formula criteria with nonmatching/blank headings are not implemented and return
`#VALUE!`; they require relative formula evaluation for each database record.

### Aggregates and statistics

- `SUM`
- `AVERAGE`
- `MIN`
- `MAX`
- `COUNT`
- `COUNTA`
- `PRODUCT`
- `SUMPRODUCT`
- `MEDIAN`
- `RANK`
- `SUBTOTAL`
- `STDEV`
- `STDEV.S`
- `STDEVP`
- `STDEV.P`
- `VAR`
- `VAR.S`
- `VARP`
- `VAR.P`
- `AVERAGEA`
- `CORREL`
- `NORMDIST`
- `NORM.DIST`
- `NORMSDIST`
- `NORM.S.DIST`
- `COVAR`
- `COVARIANCE.P`
- `COVARIANCE.S`

### Conditional aggregates

- `COUNTIF`
- `SUMIF`
- `COUNTIFS`
- `SUMIFS`
- `AVERAGEIF`
- `AVERAGEIFS`

### Logical and errors

- `IF`
- `AND`
- `OR`
- `NOT`
- `IFERROR`
- `ISBLANK`
- `ISNUMBER`
- `ISTEXT`
- `ISLOGICAL`
- `ISERROR`
- `N`
- `T`
- `CHOOSE`
- `NA`
- `ISNA`

### Math

- `ABS`
- `ROUND`
- `ROUNDUP`
- `ROUNDDOWN`
- `INT`
- `MOD`
- `POWER`
- `SQRT`
- `SIGN`
- `CEILING`
- `FLOOR`
- `TRUNC`
- `EXP`
- `LN`
- `LOG`
- `LOG10`
- `PI`
- `RAND`
- `RANDBETWEEN`

### Text

- `LEN`
- `LEFT`
- `RIGHT`
- `MID`
- `TRIM`
- `UPPER`
- `LOWER`
- `PROPER`
- `CONCAT`
- `CONCATENATE`
- `TEXTJOIN`
- `VALUE`
- `TEXT`
- `HYPERLINK`
- `EXACT`
- `FIND`
- `REPT`

### Lookup and position

- `INDEX`
- `MATCH`
- `VLOOKUP`
- `XLOOKUP`
- `HLOOKUP`
- `ROW`
- `COLUMN`
- `ROWS`
- `FILTER`
- `UNIQUE`
- `SORT`
- `LOOKUP`
- `OFFSET`
- `INDIRECT`
- `CELL`
- `GETPIVOTDATA`

### Dates (1900 serial system)

- `DATE`
- `YEAR`
- `MONTH`
- `DAY`
- `EDATE`
- `EOMONTH`
- `WEEKDAY`
- `YEARFRAC`
- `DATEVALUE`
- `DAYS360`
- `NETWORKDAYS`
- `WORKDAY`
- `TODAY`
- `NOW`

### Financial

- `PMT`
- `PV`
- `IRR`
- `NPV`
- `XNPV`
- `XIRR`
- `RRI`


`TODAY` is the 1900 serial of the tick instant's UTC date. `NOW` adds the
time-of-day fraction. Import replays a cached `TODAY()` or `NOW()` serial
as that instant and does not read a clock. A workbook whose stored
`YEARFRAC(TODAY(), …)` results all agree on one serial uses that Excel
calculation date instead. `RAND` still will not match
Excel's generator. `RAND` and `RANDBETWEEN` are deterministic in the tick
number and the calling cell: the same tick replays the same value, and a new
tick changes it. `OFFSET` refuses a result outside the grid with `#REF!` and
a height or width over 1,000,000 cells with `#NUM!`. `INDIRECT` of A1 text, including text a formula produces and a whole
column or row, is that reference. R1C1, 3D references and an external
workbook are `#REF!`.

`GETPIVOTDATA(data_field, pivot_cell, [field, item], ...)` returns the sum
of that data field over the pivot cache. The pivot cell must lie inside a
pivot table, or the result is `#REF!`. Field and item arguments are pairs;
a leftover argument is `#REF!`. Names match without regard to case, and one
surrounding space on a name is ignored only when the exact text does not
match. An unknown field or item is `#REF!`. A page-field selection still
applies when the formula does not name it. A date-between filter applies
when its field is on the row, column, or page axis, inclusive of the bounds
stored with the filter. Missing numeric cache values are not added. A visible
total with no numbers is 0. A data field whose subtotal is not sum is `#VALUE!`.

`TEXTJOIN` joins scalar and bounded range arguments in row order, can skip blanks
and empty strings, propagates errors, and refuses output beyond 32,767 UTF-16 units.

`TEXT` formats a number with one code, compared without regard to case:
`General`, `0`, `0.00`, `#`, `#,##0`, `#,##0.00`, `0%`, `0.00%`, `yyyy-mm-dd`,
or `mm/dd/yyyy`, or `mm/dd/yy`. Any other code, including a literal suffix such as `0.0x`,
is `#VALUE!`. `#` rounds half away from zero to an integer and shows nothing
for zero. Date codes use the 1900 serial, including the fictitious 1900-02-29.
`HYPERLINK` returns its friendly name, or the link when the name is omitted,
and does not fetch the target. `RANK` is a competition rank over numbers in
the reference (ties share a rank and the next rank is skipped); a zero or
omitted order ranks the largest first. `RRI` is `(fv/pv)^(1/nper)-1`.

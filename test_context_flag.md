# Test Plan for `-C` Context Flag

## Feature Description
Add a `-C NUM` flag to `ringboard search` that shows NUM entries before and after each matching entry.

## Test Cases

### Test 1: Basic context search
```bash
# Setup: clipboard with entries 1-10
# Entry 5 contains "target_text"
ringboard search -C 2 target_text
```
Expected output:
```
--- ENTRY 3 ---
...
--- ENTRY 4 ---
...
--- ENTRY 5 ---
target_text
--- ENTRY 6 ---
...
--- ENTRY 7 ---
...
```

### Test 2: Multiple matches with context
```bash
# Setup: Entries 3 and 8 match
ringboard search -C 1 "pattern"
```
Expected output:
```
--- ENTRY 2 ---
...
--- ENTRY 3 ---
pattern
--- ENTRY 4 ---
...
--
--- ENTRY 7 ---
...
--- ENTRY 8 ---
pattern
--- ENTRY 9 ---
...
```

Note: `--` separator between non-contiguous context groups.

### Test 3: Context at boundaries
```bash
# Setup: First entry matches
ringboard search -C 2 "first"
```
Expected: Shows entry 1, 2, 3 (no entries before first).

### Test 4: Overlapping contexts
```bash
# Setup: Entries 5 and 7 match
ringboard search -C 2 "text"
```
Expected: Shows entries 3-9 continuously (contexts overlap).

## Implementation Notes
- When `-C` is not provided, behavior is unchanged (original character-level context)
- When `-C NUM` is provided, show NUM entry-level context before/after matches
- Separator `--` printed between non-contiguous groups
- Works with both bucketed and file entries

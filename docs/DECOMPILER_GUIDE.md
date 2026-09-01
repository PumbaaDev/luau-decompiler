# How to use the decompiler (in detail)

This is the practical, step-by-step guide to `luau-decompiler`, and a straight
answer to the confusion about "which process is the decompiler and why isn't it
on port 8080".

---

## 0. The one thing to understand first

**Your decompiler is a command-line tool, not a server.**

It is a single Windows executable, `target\release\luau-decompiler.exe`. You run
it on a bytecode file and it prints (or writes) the Luau source. There is no
"decompiler server" to start, no port to connect to, and no `/api/decompile`
endpoint that belongs to it. When it is "not running", that is normal: it is a
tool you invoke, like `git` or `curl`, not a daemon that sits listening.

(There is an empty `crates/luau-server` folder in the repo, left over from an old
HTTP wrapper. It has no code and builds no binary. Ignore it.)

### So what were those processes you saw?

You saw three things and only one of them is yours:

| What you saw | What it actually is | Yours? |
|---|---|---|
| `luau-decompiler.exe` in `target\release\` | Your Rust decompiler (the CLI tool) | **Yes** — but it only runs while a command is executing, then exits |
| `python collect.py` on port **8080** | A separate Python script (not in this repo). 8080 is your PumbaHub in-game bridge / a data collector, unrelated to decompiling | No |
| `Decompiler.exe` (PID under `Potassium.exe`) on port **56535** | **Potassium's OWN built-in decompiler.** Every executor ships one; Potassium spawns it as a helper. Blank command line because it is Potassium's internal child process | No |

So: a `GET /api/decompile` on 8080 returning `{"status":"ok"}` was never your
decompiler answering. 8080 is `collect.py`. And `Decompiler.exe` is Potassium's
own thing. **Neither is the Rust tool.** To use *your* decompiler you run the CLI
directly, as below.

---

## 1. Where it is and how to (re)build it

- Binary: `C:\Users\jep\Downloads\luau-decompiler-v2\target\release\luau-decompiler.exe`
- Opcode database (important, see 3): `opmap_db\roblox_v9_live.json`

Rebuild after any code change:

```bash
cargo build --release
```

> On Windows, if the build fails at the link step with "file in use", a previous
> `luau-decompiler.exe` is still running. Kill it first:
> `taskkill /F /IM luau-decompiler.exe` (do NOT kill `Decompiler.exe` — that is
> Potassium's, leave it alone).

---

## 2. Decompile one file (the 90% case)

```bash
target\release\luau-decompiler.exe path\to\script.bin -o out.lua
```

- `script.bin` is raw Luau bytecode (a `.bin` / `.luac` dump).
- `-o out.lua` writes the result to a file. Omit `-o` to print to the screen.
- The equivalent explicit form is `luau-decompiler.exe decompile script.bin -o out.lua`;
  the bare shortcut above does the same thing.

The output starts with a few header comments (tool version, opcode source, proto
count) and then the reconstructed Luau. Example, run on a real corpus file:

```
-- Luau Decompiler v0.2.0
-- Opcode remapping applied (77 opcodes detected)
-- opmap source: database entry "roblox_v9_live" ...
local tbl = {}
local function cellName(arg1, arg2)
    return (("C" .. arg1) .. ",") .. arg2
end
```

---

## 3. The opcode database — why decodes are correct

Roblox ships a **shuffled** opcode set: every client version permutes the VM's
opcodes, so raw bytecode cannot be read without knowing that version's mapping.
`opmap_db\roblox_v9_live.json` holds the mapping **measured** off the client's own
compiler (77 opcodes). When it applies to a file it is used exactly and no
guessing runs.

**Good news: the CLI loads this database automatically.** You saw it in the run
above (`opmap source: database entry "roblox_v9_live"`) even without passing a
flag. So for a single file you usually do not need to think about it.

For **batch** runs and for **disassemble**, pass it explicitly so there is no doubt:

```bash
--opmap-db opmap_db\roblox_v9_live.json
```

---

## 4. Batch a whole folder + measure quality

To decompile every `.bin` in a folder and get a per-file quality report:

```bash
target\release\luau-decompiler.exe batch <input-folder> --out-dir <fresh-out-folder> --opmap-db opmap_db\roblox_v9_live.json
```

- It writes one `.lua` per input into a timestamped run folder under `--out-dir`.
- It prints a summary line: `Semantic: N/M clean (P%), K defects`.
- **It writes `SEMANTIC_REPORT.tsv`** into the run folder: one row per defect with
  `file / proto / check / line / detail`. That file is the ground truth for what
  is wrong and where. (The reference corpus is `target\union1324`, 1,323 files.)

> Measuring rule: always use a **fresh, empty** `--out-dir`. `batch` skips files
> that already exist ("resumed"), so measuring into an old folder mixes stale
> output and the numbers are meaningless. A new folder name each time avoids this.

---

## 5. Disassemble (see the raw opcodes)

```bash
target\release\luau-decompiler.exe disassemble script.bin --opmap --opmap-db opmap_db\roblox_v9_live.json
```

> **Pass BOTH `--opmap` AND `--opmap-db`.** With `--opmap` alone the listing
> misassigns the AUX-carrying opcodes and silently desynchronises — it looks
> plausible and is wrong. The DB is what makes the listing exact.

Other read-only subcommands:

- `info script.bin` — bytecode metadata (version, proto/constant counts).
- `validate out.lua` — syntax-check a `.lua`/`.luau` file (is the output valid Luau?).
- `compare a.lua b.lua` — line diff + similarity between two sources.
- `watch <folder>` — auto-decompile any new `.bin` dropped into a folder.

---

## 6. Decompile a LIVE game via Potassium (the real pipeline)

Your decompiler reads a `.bin` file. Potassium (the executor) is what pulls the
bytecode out of a running game. So the pipeline is three steps:

**Step 1 — capture the bytecode from the live game.** Run this Luau in the game
via Potassium (through the Potassium MCP on port **8225**, or its exec bridge).
`getscriptbytecode` is the key function:

```lua
-- find the target script and return its bytecode, base64-encoded
local HS = game:GetService("HttpService")
for _, s in ipairs(getscripts()) do
    if s.Name == "HUD" and (s:IsA("LocalScript") or s:IsA("ModuleScript")) then
        local bc = getscriptbytecode(s)         -- raw bytecode
        return HS:JSONEncode({ name = s:GetFullName(),
                               b64  = crypt.base64encode(bc) })
    end
end
```

(Alternative that skips HTTP size limits: `writefile("capture/HUD.luac", getscriptbytecode(s))`
writes straight into the Potassium workspace folder, which you can read from disk.)

**Step 2 — turn what came back into a `.bin` file.** Base64-decode the `b64`
field and save the bytes as `HUD.bin` (or just use the `.luac` the filesystem
path wrote).

**Step 3 — decompile it with your tool:**

```bash
target\release\luau-decompiler.exe HUD.bin -o HUD.lua
```

That is the whole "decompile the live game" flow. Nothing about it needs a
decompiler server: Potassium gets the bytes, the CLI reads the file. The phone
AI can do step 1 for you via its `use_mcp` tool against the `potassium` server,
then step 3 via `run_command`.

> If nothing runs in-game: Potassium only executes while a Roblox client is
> actually connected. Check Potassium is running and a game is open first.

---

## 7. Common errors and gotchas

- **"It's not on port 8080 / the endpoint returns empty."** Correct — that is not
  your decompiler (see section 0). The decompiler is a CLI; run it directly.
- **Link error on rebuild ("file in use").** Kill the old `luau-decompiler.exe`
  (`taskkill /F /IM luau-decompiler.exe`). Never kill Potassium's `Decompiler.exe`.
- **Disassembly looks plausible but is wrong.** You passed `--opmap` without
  `--opmap-db`. Pass both.
- **Batch quality numbers look off / too good.** You measured into a non-empty
  `--out-dir` and it resumed old files. Use a fresh folder.
- **Do not read register names off the disassembler's printer** — it invents
  names like `R{a+1}`. Trust the bytecode / the decompiled output.

---

## 8. Cheat sheet

```bash
# one file
target\release\luau-decompiler.exe script.bin -o out.lua

# whole folder + quality report (fresh out-dir!)
target\release\luau-decompiler.exe batch target\union1324 --out-dir target\run_new --opmap-db opmap_db\roblox_v9_live.json
#   -> read target\run_new\run_*\SEMANTIC_REPORT.tsv

# raw opcodes (BOTH flags)
target\release\luau-decompiler.exe disassemble script.bin --opmap --opmap-db opmap_db\roblox_v9_live.json

# metadata / syntax-check / diff
target\release\luau-decompiler.exe info script.bin
target\release\luau-decompiler.exe validate out.lua
target\release\luau-decompiler.exe compare a.lua b.lua

# rebuild
cargo build --release
```

<div align="center">

# 🔬 APB SoC Verification

### UVM verification environment for an APB peripheral subsystem, tested with the bugs it should catch

*If you never inject a bug, you don't know that a passing regression means anything.*

<br/>

![SystemVerilog](https://img.shields.io/badge/SystemVerilog-IEEE_1800-1f6feb?style=for-the-badge&logo=v&logoColor=white)
![UVM](https://img.shields.io/badge/UVM-IEEE_1800.2-7c3aed?style=for-the-badge)
![Python](https://img.shields.io/badge/Python-3-3776AB?style=for-the-badge&logo=python&logoColor=white)
![Verilator](https://img.shields.io/badge/Verilator-5.020_|_5.052-e11d48?style=for-the-badge)

![pyuvm](https://img.shields.io/badge/pyuvm-5.0.0-0891b2?style=flat-square)
![cocotb](https://img.shields.io/badge/cocotb-1.9.2-0891b2?style=flat-square)
![SVA](https://img.shields.io/badge/SVA-23_properties-f59e0b?style=flat-square)
![RAL](https://img.shields.io/badge/RAL-JSON--generated-0284c7?style=flat-square)
![License](https://img.shields.io/badge/license-MIT-64748b?style=flat-square)

![Regression](https://img.shields.io/badge/regression-90%2F90_passing-16a34a?style=flat-square)
![Coverage](https://img.shields.io/badge/functional_coverage-82%2F82_bins-16a34a?style=flat-square)
![Mutations](https://img.shields.io/badge/injected_bugs-20%2F20_detected-16a34a?style=flat-square)
![SVA activation](https://img.shields.io/badge/SVA_fired_on_target-21%2F21-16a34a?style=flat-square)

[**Results**](#-recorded-results) · [**Architecture**](#%EF%B8%8F-testbench-architecture) · [**Mutation testing**](#-mutation-testing-checking-the-checkers) · [**Quick start**](#-quick-start) · [**Limitations**](#%EF%B8%8F-limitations)

</div>

---

## 📌 TL;DR

A complete **UVM verification environment** for a multi-peripheral **APB subsystem** (UART, timer, GPIO, interrupt controller, address decoder). It is built in two ways:

| | **Main bench: pyuvm + cocotb** | **Native bench: SystemVerilog UVM** |
|---|---|---|
| Language | Python UVM (pyuvm 5.0) on cocotb 1.9.2 | IEEE 1800.2 SV-UVM (Accellera 2020.3.1) |
| Simulator | Verilator 5.020 | Verilator 5.052 (Docker) |
| Agents | APB master, UART serial, GPIO | APB master |
| Checking | Cycle-level lockstep reference model, independent UART decoder, directed spec checks, 23 SVA | Register-level scoreboard, SVA |
| Coverage | **82 functional bins** | 10 bins |
| RAL | ✅ generated from JSON | — |
| Tests | 9 | 2 |

**The main idea:** the environment is tested against **20 deliberately injected defects** (12 RTL functional bugs, 2 RTL protocol bugs, 6 APB driver protocol bugs). It catches all of them, and **every SVA property that can fire in 2-state simulation fires on the bug it targets.**

---

## 🧩 Design under test

`periph_soc` is a peripheral subsystem with one APB completer port. The testbench acts as the APB requester, standing in for a CPU.

```mermaid
flowchart LR
    TB(["🧪 Testbench<br/>APB requester"]):::tb
    DEC{"Address decoder<br/>paddr[15:12]"}:::dec
    UART["📡 UART · 0x0000<br/>8-deep TX/RX FIFOs<br/>parity · baud divider"]:::uart
    TIM["⏱️ TIMER · 0x1000<br/>prescaler · compare<br/>one-shot / auto-reload"]:::tim
    GPIO["🔌 GPIO · 0x2000<br/>16 pins · edge IRQs"]:::gpio
    ERR["⛔ Unmapped<br/>PSLVERR, data 0"]:::err
    INTC["🔔 INTC · 0xF000<br/>pending · enable mask"]:::intc
    SER(["Serial line"]):::io
    PINS(["GPIO pins"]):::io

    TB == "APB: psel · penable · pwrite<br/>paddr · pwdata" ==> DEC
    DEC --> UART & TIM & GPIO & ERR
    DEC --> INTC
    UART -- irq --> INTC
    TIM -- irq --> INTC
    GPIO -- irq --> INTC
    INTC == "irq" ==> CPU(["🧪 Testbench<br/>irq monitor"]):::tb
    UART <-. "uart_tx / uart_rx" .-> SER
    GPIO <-. "in / out / oe" .-> PINS

    classDef tb fill:#4f46e5,stroke:#312e81,color:#ffffff,stroke-width:2px
    classDef dec fill:#f59e0b,stroke:#92400e,color:#111827,stroke-width:2px
    classDef uart fill:#0ea5e9,stroke:#075985,color:#ffffff,stroke-width:2px
    classDef tim fill:#8b5cf6,stroke:#4c1d95,color:#ffffff,stroke-width:2px
    classDef gpio fill:#10b981,stroke:#065f46,color:#ffffff,stroke-width:2px
    classDef intc fill:#ec4899,stroke:#831843,color:#ffffff,stroke-width:2px
    classDef err fill:#ef4444,stroke:#7f1d1d,color:#ffffff,stroke-width:2px
    classDef io fill:#334155,stroke:#0f172a,color:#ffffff
```

<details>
<summary><b>📖 Memory map (generated from <code>regs/periph_soc.json</code>)</b></summary>

<br/>

| Block | Base | Registers | Notable behaviour |
|---|---|---|---|
| **UART** | `0x0000` | DATA, STATUS, CTRL, BAUD, IRQ_EN, IRQ_ST, RX_COUNT | One APB wait state per access · baud divider clamps to 4 · sticky W1C error flags |
| **TIMER** | `0x1000` | CTRL, PRESC, COMPARE, COUNT, STATUS | Counts every PRESC+1 clocks · one-shot or auto-reload · W1C match flag |
| **GPIO** | `0x2000` | OUT, OE, IN, RISE_EN, FALL_EN, IRQ_ST | Synchronised inputs · per-pin rise/fall interrupts · W1C edge flags |
| **INTC** | `0xF000` | PENDING, ENABLE | ORs the masked UART / timer / GPIO interrupts onto `irq` |
| *other* | — | — | `PSLVERR`, read data 0, no wait states |

Full field-level map: [`docs/register_map.md`](docs/register_map.md)

</details>

---

## 🏗️ Testbench architecture

The main bench is a standard UVM layering (sequences → sequencers → drivers → DUT → monitors → checkers) with **three kinds of checking**, so a mistake in one checker doesn't hide a bug.

```mermaid
flowchart TB
    subgraph SPEC["📜 Single source of truth"]
        JSON[("regs/periph_soc.json")]:::spec
    end

    subgraph STIM["🎲 Stimulus layer"]
        TESTS["9 uvm_tests<br/>Register · UartTx · UartRx · Timer<br/>Gpio · Intc · Stress · RalRegister · RalW1c"]:::stim
        SEQ["Constrained-random sequences<br/>seeded via RANDOM_SEED"]:::stim
        RAL["RAL model<br/>uvm_reg_block / uvm_reg / uvm_reg_field"]:::spec
        TESTS --> SEQ
        RAL --> SEQ
    end

    subgraph AGENTS["🤖 Agents"]
        APB["APB agent<br/>driver · monitor · sequencer"]:::agent
        UARTA["UART agent<br/>RX line driver · TX monitor"]:::agent
        GPIOA["GPIO agent<br/>input pin driver"]:::agent
    end

    DUT[["⚙️ periph_soc RTL"]]:::dut
    SVA["🛡️ 23 bound SVA properties<br/>APB protocol + peripheral rules"]:::sva

    subgraph CHECK["✅ Checking layer"]
        REF["Cycle-level<br/>reference model<br/>tb/refmodel.py"]:::chk
        SCB["Lockstep scoreboard<br/>compares 7 outputs, every cycle"]:::chk
        TXC["Independent UART TX decoder<br/>parity · stop bit · ordering"]:::ind
        DIR["Spec-derived sequence checks<br/>W1C · overrun · glitch · reload"]:::ind
        REF --> SCB
    end

    COV["📊 Functional coverage<br/>82 bins, merged across seeds"]:::cov
    RES[("results/*.json")]:::cov

    JSON -- "tools/reggen.py" --> RAL
    SEQ --> APB & UARTA & GPIOA
    APB & UARTA & GPIOA --> DUT
    DUT --> SCB & TXC & DIR
    DUT -. bind .-> SVA
    SCB & TXC & DIR & SVA --> COV --> RES

    classDef spec fill:#0284c7,stroke:#0c4a6e,color:#ffffff,stroke-width:2px
    classDef stim fill:#4f46e5,stroke:#312e81,color:#ffffff,stroke-width:2px
    classDef agent fill:#7c3aed,stroke:#4c1d95,color:#ffffff,stroke-width:2px
    classDef dut fill:#f59e0b,stroke:#92400e,color:#111827,stroke-width:3px
    classDef sva fill:#dc2626,stroke:#7f1d1d,color:#ffffff,stroke-width:2px
    classDef chk fill:#059669,stroke:#064e3b,color:#ffffff,stroke-width:2px
    classDef ind fill:#14b8a6,stroke:#134e4a,color:#ffffff,stroke-width:2px
    classDef cov fill:#db2777,stroke:#831843,color:#ffffff,stroke-width:2px
    style SPEC fill:transparent,stroke:#0284c7,stroke-width:2px,stroke-dasharray:6 4
    style STIM fill:transparent,stroke:#4f46e5,stroke-width:2px,stroke-dasharray:6 4
    style AGENTS fill:transparent,stroke:#7c3aed,stroke-width:2px,stroke-dasharray:6 4
    style CHECK fill:transparent,stroke:#059669,stroke-width:2px,stroke-dasharray:6 4
```

### Three checkers that don't share the same assumptions

| Checker | What it does | Independent of the reference model? |
|---|---|:---:|
| 🟢 **Lockstep scoreboard** | A cycle-level Python model steps alongside the DUT and compares `pready`, `pslverr`, `uart_tx`, `gpio_out`, `gpio_oe`, `irq` **every cycle**, and `prdata` on every completed read | ❌ |
| 🟢 **UART TX decoder** | Samples the serial line mid-bit. Every byte accepted into the TX FIFO must appear in order, with correct parity and stop bit, and nothing else may appear | ✅ |
| 🟢 **Spec-derived sequence checks** | RX data equals the frames driven · parity/frame errors set on bad frames · overrun keeps the first 8 bytes · glitches rejected · W1C semantics · timer one-shot vs auto-reload | ✅ |
| 🔴 **SVA** | APB protocol and peripheral properties bound into the RTL, compiled with `--assert` on every run | ✅ |

The RTL and the reference model were written by the same author from the same spec, so a misreading shared by both would get past the lockstep scoreboard. The independent checkers are there to catch exactly that.

### What happens every clock cycle

```mermaid
sequenceDiagram
    autonumber
    participant D as 🤖 Drivers
    participant R as ⚙️ periph_soc RTL
    participant M as 🐍 Reference model
    participant S as 🟢 Lockstep scoreboard
    participant A as 🛡️ SVA

    rect rgba(79, 70, 229, 0.12)
    D->>R: drive inputs after posedge clk
    end
    rect rgba(5, 150, 105, 0.12)
    Note over S: ReadOnly phase
    S->>R: sample inputs + 7 outputs
    S->>M: outputs(inputs)
    M-->>S: expected outputs
    alt mismatch
        S-->>S: log error, raise failed event, test stops early
    else match
        S->>M: step(inputs)
    end
    end
    rect rgba(220, 38, 38, 0.12)
    A-->>A: evaluate properties concurrently, print SVA_FAIL on violation
    end
```

---

## 🛡️ SystemVerilog Assertions

23 concurrent properties in [`sva/soc_sva.sv`](sva/soc_sva.sv), bound to the DUT by [`sva/sva_bind.sv`](sva/sva_bind.sv). The APB protocol properties follow the APB state machine:

```mermaid
stateDiagram-v2
    direction LR
    [*] --> IDLE
    IDLE --> SETUP: psel=1, penable=0
    SETUP --> ACCESS: penable=1
    ACCESS --> ACCESS: pready=0 (wait state)
    ACCESS --> SETUP: pready=1, back-to-back
    ACCESS --> IDLE: pready=1, no transfer

    note right of SETUP
        APB_FIRST_CYCLE_IS_SETUP
        APB_SETUP_THEN_ACCESS
        APB_PENABLE_NEEDS_PSEL
    end note
    note right of ACCESS
        APB_ACCESS_HOLDS_UNTIL_READY
        APB_ADDR_CTRL_STABLE
        APB_WDATA_STABLE
        APB_PREADY_ONLY_IN_ACCESS
        APB_SLVERR_ONLY_IN_ACCESS
        APB_PENABLE_DROPS_AFTER_READY
    end note

    classDef idle fill:#475569,color:#ffffff,stroke:#0f172a
    classDef setup fill:#4f46e5,color:#ffffff,stroke:#312e81
    classDef access fill:#059669,color:#ffffff,stroke:#064e3b
    class IDLE idle
    class SETUP setup
    class ACCESS access
```

| Group | Count | Examples |
|---|:---:|---|
| 🔵 APB protocol | 9 | the properties shown above |
| 📡 UART | 6 | `UART_TX_PARITY_CORRECT` · `UART_RX_SAMPLES_MID_BIT` · `UART_TX_FULL_AT_DEPTH` · `UART_BAUD_AT_LEAST_4` |
| ⏱️ Timer | 2 | `TIMER_TICK_EVERY_PRESC_PLUS_1` · `TIMER_MATCH_RESTARTS_AT_0` |
| 🔌 GPIO | 2 | `GPIO_W1C_CLEARS_ONLY_WRITTEN_BITS` · `GPIO_EDGE_ALWAYS_CAPTURED` |
| 🔔 Decoder / INTC | 2 | `SOC_DECODE_HOLE_ERRORS` · `SOC_IRQ_IS_MASKED_OR` |
| ⚪ No-X (4-state only) | 2 | `APB_NO_X_ON_REQUEST` · `SOC_NO_X_ON_OUTPUTS` |

> [!NOTE]
> **Built for the simulator I actually had.** A probe ([`verilator_5020_sva_support_probe.log`](docs/evidence/2026-10-15/verilator_5020_sva_support_probe.log)) showed that Verilator 5.020 rejects `##N`, named sequences, `[*N]` and `##[m:n]`. So every property uses only `|->`, `|=>`, `$past`, `$stable` and `$isunknown`, and multi-cycle rules are written as chains of one-cycle implications.

---

## 🗂️ Register Abstraction Layer: one spec, three outputs

The register map is written once, as JSON. Everything else is generated from it, and the RAL tests check the hand-written RTL against it.

```mermaid
flowchart LR
    J[("📜 regs/periph_soc.json")]:::src --> G{{"⚙️ tools/reggen.py"}}:::gen
    G --> P["🐍 tb/ral_model.py<br/>pyuvm RAL classes"]:::out
    G --> C["🧱 sw/periph_soc_regs.h<br/>C firmware header"]:::out
    G --> MD["📖 docs/register_map.md<br/>documentation"]:::out
    P --> T["🧪 RAL sequences<br/>reset · bit-bash · mirror · W1C<br/>367 checks per run"]:::test
    RTL[["⚙️ Hand-written RTL"]]:::rtl --> T
    T --> V{"RTL matches spec?"}:::gen
    G -. "--check: generated files<br/>must be up to date" .-> J

    classDef src fill:#0284c7,stroke:#0c4a6e,color:#ffffff,stroke-width:2px
    classDef gen fill:#f59e0b,stroke:#92400e,color:#111827,stroke-width:2px
    classDef out fill:#7c3aed,stroke:#4c1d95,color:#ffffff,stroke-width:2px
    classDef test fill:#059669,stroke:#064e3b,color:#ffffff,stroke-width:2px
    classDef rtl fill:#475569,stroke:#0f172a,color:#ffffff,stroke-width:2px
```

**Working around pyuvm 5.0:** its `uvm_reg_predictor`, backdoor access and built-in register sequences raise `NotImplementedError`. [`tb/ral.py`](tb/ral.py) adds small replacements in the style of the UVM built-ins: auto-predict through the map, a "backdoor" that reads the RTL signals named in the JSON through cocotb handles, and hand-written reset, bit-bash, mirror and W1C sequences.

---

## 🧨 Mutation testing: checking the checkers

[`tools/regress.py`](tools/regress.py) rebuilds the DUT with each bug enabled, runs the whole suite, and records which tests failed and which SVA properties fired.

```mermaid
flowchart LR
    START(["regress.py<br/>--bugs --sva-bugs --ral-bugs"]):::start --> PICK["Pick next mutation"]:::step
    PICK --> KIND{"Kind?"}:::dec
    KIND -- "RTL bug" --> BUILD["Rebuild DUT with<br/>+define+BUG_*"]:::step
    KIND -- "Driver bug" --> ENV["Set APB_DRV_BUG=<br/>protocol violation"]:::step
    BUILD & ENV --> RUN["Run test suite<br/>SVA compiled with --assert"]:::step
    RUN --> Q{"Any test failed<br/>or SVA fired?"}:::dec
    Q -- yes --> KILL["✅ Caught<br/>record tests + properties"]:::ok
    Q -- no --> MISS["❌ Missed<br/>a hole in the checks"]:::bad
    KILL & MISS --> MORE{"More<br/>mutations?"}:::dec
    MORE -- yes --> PICK
    MORE -- no --> OUT[("📄 regression.json<br/>+ full log")]:::out

    classDef start fill:#4f46e5,stroke:#312e81,color:#ffffff,stroke-width:2px
    classDef step fill:#334155,stroke:#0f172a,color:#ffffff
    classDef dec fill:#f59e0b,stroke:#92400e,color:#111827
    classDef ok fill:#16a34a,stroke:#14532d,color:#ffffff,stroke-width:2px
    classDef bad fill:#dc2626,stroke:#7f1d1d,color:#ffffff,stroke-width:2px
    classDef out fill:#db2777,stroke:#831843,color:#ffffff
```

```mermaid
pie showData
    title Injected defects by class (20 total, all detected)
    "RTL functional bugs" : 12
    "APB requester (driver) violations" : 6
    "APB completer (RTL) violations" : 2
```

<details open>
<summary><b>🐞 Every functional bug and what caught it</b> (from <a href="results/2026-10-15/regression_full.log"><code>regression_full.log</code></a>)</summary>

<br/>

| Injected bug | Failure class | Caught by tests | SVA that fired |
|---|---|---|---|
| `BUG_UART_PARITY` | Data path | UartTx, Stress | `UART_TX_PARITY_CORRECT` |
| `BUG_UART_FIFO_DEPTH` | Off-by-one capacity | UartTx, Stress | `UART_TX_FULL_AT_DEPTH` |
| `BUG_RX_SAMPLE` | RX sampling point | UartRx, Stress, RalW1c | `UART_RX_SAMPLES_MID_BIT` |
| `BUG_UART_W1C` | Register semantics | UartRx, RalW1c | `UART_W1C_CLEARS_OVERRUN` |
| `BUG_UART_NOWAIT` | Protocol: missing wait state | 7 of 9 tests | `UART_FIRST_ACCESS_CYCLE_WAITS` |
| `BUG_UART_BAUD_CLAMP` | Register semantics | UartTx | `UART_BAUD_AT_LEAST_4` |
| `BUG_TIMER_RELOAD` | Counter behaviour | Timer | `TIMER_MATCH_RESTARTS_AT_0` |
| `BUG_TIMER_PRESC` | Counter behaviour | Intc | `TIMER_TICK_EVERY_PRESC_PLUS_1` |
| `BUG_GPIO_W1C` | W1C clears everything | Gpio, RalW1c | `GPIO_W1C_CLEARS_ONLY_WRITTEN_BITS` |
| `BUG_GPIO_EDGE_LOST` | Edge lost during W1C | Gpio | `GPIO_EDGE_ALWAYS_CAPTURED` |
| `BUG_INTC_MASK` | Interrupt routing | Gpio, Intc, Stress, RalW1c | `SOC_IRQ_IS_MASKED_OR` |
| `BUG_DECODE` | Address aliasing | Register | `SOC_DECODE_HOLE_ERRORS` |

**Each checker on its own:** with the lockstep scoreboard switched off (`LOCKSTEP=0`) and SVA non-fatal, `RalW1cTest` alone still catches both W1C bugs, e.g. `W1C of bit 2: uart_irq_st read 0x3c, mirror 0x38`.

</details>

---

## 📈 Recorded results

Run on 2026-10-15 with `python3 tools/regress.py --seeds 10 --bugs --bug-seeds 2 --sva-bugs --ral-bugs`

| Metric | Result | Scope |
|---|:---:|---|
| ✅ Seeded regression | **10 seeds × 9 tests = 90/90 pass** | Main pyuvm bench |
| 📊 Functional coverage | **82/82 bins (100%)** | Merged over 10 seeds; says nothing about whether the bin set itself is complete |
| 🧨 Functional RTL bugs | **12/12 caught** | The chosen mutation set |
| 🔵 Protocol bugs (driver + RTL) | **8/8 caught by SVA** | 6 requester + 2 completer violations |
| 🛡️ SVA properties firing on their target | **21/21 testable** | 2 no-X properties can't fail on 2-state Verilator |
| 🧹 SVA failures on clean RTL | **0** | All 10 seeds |
| 🗂️ RAL register checks | **367 per run** | Reset, bit-bash, mirror, W1C |
| ⚙️ Native SV-UVM | **UVM_ERROR 0 / UVM_FATAL 0**, 450 transfers checked | Verilator 5.052 + Accellera UVM 2020.3.1 |
| 🎯 Native SV-UVM, failing first | **UVM_ERROR 26** with `+define+BUG_DECODE` | Shows the SV scoreboard really detects the bug |

```mermaid
xychart-beta
    title "Wall time per seed, full 9-test suite (seconds)"
    x-axis "Seed" [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]
    y-axis "Seconds" 0 --> 25
    bar [12.7, 13.4, 13.7, 17.7, 15.5, 20.5, 20.1, 15.4, 18.6, 19.7]
```
<sub>Measured on a shared 4-vCPU machine under load, so the spread is mostly noise from other jobs.</sub>

---

## ⚙️ The native SystemVerilog UVM bench

A second, smaller environment in [`sv_uvm/`](sv_uvm), written in standard IEEE 1800.2 SystemVerilog UVM so the same DUT runs on an industry-style flow.

```mermaid
flowchart LR
    TOP["tb_top.sv<br/>clock · reset · run_test()"]:::top
    subgraph PKG["soc_uvm_pkg.sv"]
        direction LR
        T["soc_reg_test<br/>soc_decode_test"]:::test --> SQ["reg_rw_seq<br/>decode_err_seq"]:::seq
        SQ --> AG["apb_agent<br/>sequencer · driver · monitor"]:::agent
        AG --> SB["soc_scoreboard<br/>register-level model"]:::chk
        AG --> CV["soc_coverage<br/>10 bins"]:::cov
    end
    TOP --> T
    AG <== "apb_if" ==> DUT[["periph_soc + bound SVA"]]:::dut

    classDef top fill:#475569,stroke:#0f172a,color:#ffffff
    classDef test fill:#4f46e5,stroke:#312e81,color:#ffffff
    classDef seq fill:#6366f1,stroke:#312e81,color:#ffffff
    classDef agent fill:#7c3aed,stroke:#4c1d95,color:#ffffff
    classDef chk fill:#059669,stroke:#064e3b,color:#ffffff
    classDef cov fill:#db2777,stroke:#831843,color:#ffffff
    classDef dut fill:#f59e0b,stroke:#92400e,color:#111827,stroke-width:3px
    style PKG fill:transparent,stroke:#7c3aed,stroke-width:2px,stroke-dasharray:6 4
```

| Test | Transfers checked | Mismatches | Coverage | UVM_ERROR | UVM_FATAL | SVA_FAIL |
|---|:---:|:---:|:---:|:---:|:---:|:---:|
| `soc_reg_test` | 200 | 0 | 9/10 | 0 | 0 | 0 |
| `soc_decode_test` | 250 | 0 | 10/10 | 0 | 0 | 0 |

Verilator 5.020 can't compile the UVM library, so this bench uses the official `verilator/verilator:v5.052` Docker image with pinned digests ([`docs/sv_uvm.md`](docs/sv_uvm.md)). Ready-to-run commands for **Xcelium, Questa, VCS and EDA Playground** are in [`docs/OWNER_ACTIONS.md`](docs/OWNER_ACTIONS.md).

---

## 🚀 Quick start

**Requirements:** Python 3, Make, a C++ toolchain, Verilator ≥ 5.020

```bash
# 1. Python environment
python3 -m venv .venv && source .venv/bin/activate
pip install -r tb/requirements.txt          # cocotb 1.9.2, pyuvm 5.0.0

# 2. Run the suite once (SVA compiled in by default)
(cd tb && make SIM=verilator)

# 3. Full regression: 10 seeds + every mutation campaign
python3 tools/regress.py --seeds 10 --bugs --sva-bugs --ral-bugs

# 4. Regenerate the RAL model, C header and register docs from JSON
python3 tools/reggen.py
```

<details>
<summary><b>🔧 More options</b></summary>

<br/>

```bash
# One test, one seed (pyuvm tests are picked with ONLY_TESTS, not TESTCASE)
(cd tb && ONLY_TESTS=UartRxTest RANDOM_SEED=7 make SIM=verilator)

# Build with an injected RTL bug (separate build dir so the clean build stays intact)
(cd tb && make SIM=verilator SIM_BUILD=sim_build_parity BUG_DEFINES=+define+BUG_UART_PARITY)

# Inject an APB protocol violation from the driver, SVA reported but not fatal
(cd tb && ONLY_TESTS=RegisterTest APB_DRV_BUG=addr_change SVA_FATAL=0 make SIM=verilator)

# Native SystemVerilog UVM bench (Verilator >= 5.036, Accellera uvm-core)
UVM_HOME=/path/to/uvm-core/src sv_uvm/run_verilator.sh out/
UVM_HOME=/path/to/uvm-core/src sv_uvm/run_verilator.sh out_bug/ +define+BUG_DECODE   # failing-first run
```

</details>

---

## 📁 Repository layout

```text
UVM-SOC-VERIFICATION/
├── rtl/            ⚙️  DUT: periph_soc top, apb_uart, apb_timer, apb_gpio (bugs behind `ifdef BUG_*)
├── tb/             🐍  pyuvm bench: agents, env, scoreboard, refmodel, sequences, RAL, tests
├── sva/            🛡️  SVA properties, bind file, Verilator SVA-support probe
├── sv_uvm/         🧱  native SystemVerilog UVM bench + Verilator run script
├── regs/           📜  periph_soc.json, the single register specification
├── tools/          🔧  reggen.py (code generator) · regress.py (regression + mutation runner)
├── sw/             💾  generated C register header for firmware
├── results/        📊  machine-readable regression outputs (JSON + full log)
└── docs/           📖  verification plan, requirements matrix, register map, limitations, evidence logs
```

| Start reading here | Why |
|---|---|
| [`tb/refmodel.py`](tb/refmodel.py) | Cycle-level reference model of the whole subsystem |
| [`tb/env.py`](tb/env.py) | Lockstep scoreboard, UART decoder, coverage collector, SVA counter |
| [`sva/soc_sva.sv`](sva/soc_sva.sv) | All 23 concurrent assertions |
| [`tools/regress.py`](tools/regress.py) | How the mutation campaigns run |
| [`docs/verification_plan.md`](docs/verification_plan.md) | Feature → stimulus → coverage bin mapping |
| [`docs/requirements.md`](docs/requirements.md) | Every claim tagged VERIFIED / UNVERIFIED / BLOCKED, with evidence |

---

## 🧠 Skills demonstrated

<table>
<tr>
<td width="50%" valign="top">

**Verification methodology**
- UVM component hierarchy and phasing
- Constrained-random stimulus with reproducible seeds
- Coverage-driven closure with merging across seeds
- Reference-model and lockstep scoreboarding
- Mutation testing to measure how good the checks are

</td>
<td width="50%" valign="top">

**Languages & tooling**
- SystemVerilog RTL, SVA, native SV-UVM
- Python: pyuvm, cocotb, code generation
- UVM RAL: reset, bit-bash, mirror, W1C sequences
- Verilator flows, Docker-pinned toolchains
- Regression automation with JSON reporting

</td>
</tr>
</table>

---

## ⚠️ Limitations

Every claim above is tied to a log in [`results/`](results/2026-10-15) or [`docs/evidence/`](docs/evidence/2026-10-15). What this project does **not** show:

- **No commercial simulator run.** Both benches are verified on Verilator only, and the two no-X assertions need a 4-state simulator to mean anything.
- **No formal proof.** The SVA are checked in simulation only.
- **The SV-UVM bench is deliberately small.** It uses `$urandom` instead of `randomize()` (Verilator 5.052 failed on inline constraints) and bin counters instead of covergroups. The 82-bin and RAL results belong to the pyuvm bench.
- **The model and the RTL share an author.** The independent checkers reduce the risk of a shared misreading but don't remove it.
- **No code coverage.** Line and toggle coverage weren't collected.
- **It's a peripheral subsystem, not a CPU SoC.** The testbench plays the role of the CPU.

Full list: [`docs/limitations.md`](docs/limitations.md)

### 🛣️ Next steps
- [ ] Run `sv_uvm/` on Xcelium / Questa / VCS and turn on the no-X properties
- [ ] Formal proof of the SVA set (Jasper / VC Formal / SymbiYosys)
- [ ] Covergroups and constrained `randomize()` in the native bench, once the simulator supports them
- [ ] A larger, automatically generated mutation set
- [ ] Line and toggle coverage alongside functional coverage

---

<div align="center">

**Built by [Siddhant Singh](https://github.com/siddhaantxsingh)** · B.Tech ECE (VLSI Design & Technology), MIT Manipal

<sub>MIT License · See <a href="LICENSE">LICENSE</a></sub>

</div>

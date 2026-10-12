# Rust Briefing for C#/.NET Developers

A quick reference mapping Rust concepts to their C#/.NET equivalents, written for the SRE AI Agents codebase.

---

## 1. What is a Crate?

A **crate** is Rust's equivalent of a **C# project / .NET assembly**. It's the unit of compilation.

| C# / .NET | Rust |
|-----------|------|
| `.csproj` project | Crate |
| `.sln` solution | Workspace (`[workspace]` in `Cargo.toml`) |
| NuGet package | Published crate (crates.io) |
| Project reference | `path = "crates/common"` in `Cargo.toml` |

Two types of crates:

- **Library crate** — produces reusable code (like a C# class library). Entry point: `lib.rs`
- **Binary crate** — produces an executable (like a C# console app). Entry point: `main.rs`

In this workspace:

```
sre-agents-platform/          # root crate — has BOTH lib.rs and main.rs
  crates/common/              # library crate
  crates/store/               # library crate
  crates/azure/               # library crate
  crates/agents/              # library crate
  crates/orchestrator/        # library crate
```

## 2. Why do we need `lib.rs`?

In C#, any project can reference any other project's public types — there's no distinction between "this is a library" and "this is an executable."

Rust is stricter. **Integration tests** (files in `tests/`) are compiled as *separate crates*. They can only import from **library crates**, not from binaries.

Before `lib.rs`:

```
src/main.rs      <- binary crate (sre-agents)
src/webhook.rs   <- module inside the binary
tests/webhook_tests.rs  <- separate crate, CANNOT import from a binary
```

The tests couldn't use the real `AppState` or handlers — they had to re-implement them as copies.

After `lib.rs`:

```
src/lib.rs       <- library crate (sre_agents_platform) -- exports webhook module
src/main.rs      <- binary crate -- uses lib.rs internally
tests/webhook_tests.rs  <- imports from the library crate
```

**C# analogy:** It's like splitting a console app into a class library + a thin console project that references it. The test project references the class library.

## 3. `pub` — Visibility

Exactly like C#'s `public` keyword. Controls visibility.

```rust
pub struct AppState { ... }     // C#: public class AppState { }
pub fn healthz() { ... }       // C#: public void Healthz() { }
struct InternalThing { ... }    // C#: internal class InternalThing { }  (default)
```

Rust defaults to **private** (like C#'s `internal` — visible within the crate only). `pub` makes it visible to other crates.

There's also `pub(crate)` which is exactly C#'s `internal` — public within this crate, invisible outside.

## 4. `impl` — Attaching Methods to Structs

`impl` is how you attach methods to a struct. C# puts methods inside the class body; Rust separates data from behavior.

```csharp
// C#
public class Orchestrator {
    private PlatformConfig config;

    public Orchestrator(PlatformConfig config) {
        this.config = config;
    }

    public async Task<Report> Investigate(AlertInfo alert) {
        // ...
    }
}
```

```rust
// Rust — data and methods are separate blocks
struct Orchestrator {
    config: PlatformConfig,
}

impl Orchestrator {
    // Constructor (just a convention -- "new" is not special)
    pub fn new(config: PlatformConfig) -> Self {
        Self { config }
    }

    pub async fn investigate(&self, alert: AlertInfo) -> Result<Report, Error> {
        // ...
    }
}
```

You can have **multiple `impl` blocks** for the same struct — Rust doesn't care. You can also `impl` a trait (interface) for a struct:

```rust
// Like implementing an interface in C#
impl WorkflowStore for PgStore {
    async fn create_workflow(&self, ...) { ... }
}
```

```csharp
// C# equivalent
public class PgStore : IWorkflowStore {
    public async Task CreateWorkflow(...) { ... }
}
```

## 5. `self`, `&self`, `&mut self` — Ownership and Borrowing

This is the biggest mental shift from C#. In C#, `this` is always implicitly available and it's always a reference. In Rust, you choose **how** the method receives the instance:

| Rust | C# Equivalent | Meaning |
|------|---------------|---------|
| `&self` | `this` (normal method) | Borrow immutably — read but don't modify |
| `&mut self` | `this` in a method that mutates fields | Borrow mutably — can modify fields |
| `self` | no direct equivalent | **Takes ownership** — the caller can't use the object after this call |

```rust
impl Orchestrator {
    // Read-only access — like a C# method that doesn't modify fields
    pub fn id(&self) -> &str { &self.id }

    // Mutable access — like a C# method that modifies fields
    pub fn set_name(&mut self, name: String) { self.name = name; }

    // Takes ownership — consumes the object (rare, used for builders/destructors)
    pub fn into_parts(self) -> (Config, Router) { (self.config, self.router) }
}
```

### Deep dive: `self` (owned) — consuming the object

`self` (no `&`) means the method **takes ownership** of the object. After you call it, the original variable is **gone** — you can't use it anymore. The method consumed it.

```rust
impl DatabaseConnection {
    // Takes ownership — connection is consumed
    pub fn close(self) {
        // self is dropped at the end, cleaning up resources
    }
}

let conn = DatabaseConnection::new();
conn.close();    // conn is MOVED into close()
conn.query();    // COMPILE ERROR: conn no longer exists
```

C# equivalent would be like calling `Dispose()` and then having the compiler **physically prevent** you from touching the object again. C# can't do that — you can call a disposed object and get a runtime exception. Rust catches it at compile time.

**When you see `self` (owned) in practice:**

| Pattern | Why `self` (owned) |
|---------|--------------------|
| `.close(self)` | Resource cleanup — file handle, connection |
| `.into_string(self)` | Type conversion — consumes old type, returns new one |
| `.build(self)` | Builder pattern — consumes the builder, returns the built thing |
| `.into_parts(self)` | Destructuring — splits an object into its fields |

It's the rarest of the three. In our codebase we don't use it — everything is `&self` (read) or `&mut self` (mutate). You'll mostly see `self` in standard library types like `String::into_bytes(self)`.

### Cheat sheet

| Syntax | C# Analogy | Object after call |
|--------|------------|-------------------|
| `&self` | `this` (readonly) | Still usable |
| `&mut self` | `this` (can modify) | Still usable |
| `self` | `this` + Dispose + compiler enforced death | **Gone** |

### Why this matters in our codebase

We changed `Orchestrator::investigate` from `&mut self` to `&self`. With `&mut self`, Rust guarantees only ONE caller can use the orchestrator at a time (like a C# `lock`). We couldn't `Arc::new()` and share it across tasks. With `&self`, multiple tasks can call `investigate()` concurrently — because they're all just reading shared state (the `Arc`-wrapped store, router, etc. handle their own internal synchronization).

## 6. Traits — Interfaces on Steroids

A **trait** is Rust's version of a C# `interface` — a contract that says "any type implementing me guarantees these methods exist." Unlike C# interfaces, traits carry no data, only behavior.

### Defining a trait

```rust
// Rust
trait Investigate {
    // Required — every implementor must provide this
    fn gather_evidence(&self) -> Vec<String>;
    fn severity(&self) -> u8;

    // Default method — implementors get this for free (can override)
    fn is_critical(&self) -> bool {
        self.severity() >= 4
    }
}
```

```csharp
// C# equivalent
public interface IInvestigate {
    List<string> GatherEvidence();
    byte Severity();

    // Default interface method (C# 8+)
    bool IsCritical() => Severity() >= 4;
}
```

### `impl Trait for MyStruct` — fulfilling the contract

This is the same `impl` block you already know from section 4, but now you're attaching **trait behavior** to a struct instead of standalone methods. Side by side:

```rust
// Standalone methods — behavior that belongs to Server
impl Server {
    fn new(name: &str, cpu: f64) -> Self {
        Server { name: name.to_string(), cpu_percent: cpu }
    }

    fn is_healthy(&self) -> bool {
        self.cpu_percent < 90.0
    }
}

// Trait implementation — Server now fulfills the Investigate contract
impl Investigate for Server {
    fn gather_evidence(&self) -> Vec<String> {
        vec![format!("{} CPU at {}%", self.name, self.cpu_percent)]
    }

    fn severity(&self) -> u8 {
        if self.cpu_percent > 95.0 { 5 } else { 2 }
    }
    // is_critical() — we get the default for free, no need to implement
}
```

```csharp
// C# equivalent — both in one block
public class Server : IInvestigate {
    private string name;
    private double cpuPercent;

    // Standalone methods
    public Server(string name, double cpu) { ... }
    public bool IsHealthy() => cpuPercent < 90.0;

    // Interface implementation
    public List<string> GatherEvidence() =>
        new() { $"{name} CPU at {cpuPercent}%" };
    public byte Severity() => cpuPercent > 95.0 ? 5 : 2;
}
```

| What you write | What it means | C# analogy |
|---|---|---|
| `impl Server { ... }` | Methods that belong to Server | Methods on a class |
| `trait Investigate { ... }` | A contract / capability | `interface IInvestigate` |
| `impl Investigate for Server { ... }` | Server fulfills the contract | `class Server : IInvestigate` |

### Using traits in function signatures

Once a struct implements a trait, you can write functions that accept **any type implementing that trait** — this is where traits unlock polymorphism. The `&` / `&mut` / owned rules from section 5 apply here too:

```rust
// Borrows immutably — caller keeps the value
fn log_findings(target: &impl Investigate) {
    for finding in target.gather_evidence() {
        println!("Finding: {}", finding);
    }
}

// Borrows mutably — caller keeps the value but gives exclusive access
fn escalate_if_critical(target: &mut impl Investigate) {
    if target.is_critical() {
        println!("ESCALATING! Severity: {}", target.severity());
    }
}

// Takes ownership — caller can't use the value afterward
fn archive(target: impl Investigate) {
    let evidence = target.gather_evidence();
    println!("Archived {} findings", evidence.len());
    // target is dropped here — caller can't use it anymore
}
```

```rust
fn main() {
    let mut srv = Server::new("web-prod-01", 97.5);

    log_findings(&srv);              // borrows — srv still usable
    escalate_if_critical(&mut srv);  // mutable borrow — srv still usable
    archive(srv);                    // MOVES srv — can't use it after this
    // srv.is_healthy();             // compile error: value used after move
}
```

```csharp
// C# equivalent — no ownership distinction, everything is a reference
void LogFindings(IInvestigate target) { ... }
void EscalateIfCritical(IInvestigate target) { ... }
void Archive(IInvestigate target) { ... }

var srv = new Server("web-prod-01", 97.5);
LogFindings(srv);           // srv still usable
EscalateIfCritical(srv);    // srv still usable
Archive(srv);               // srv STILL usable — C# has no ownership
```

### Trait method signatures vs function parameter signatures

There are two separate `&self` decisions happening, and they control different things:

| Where | Controls | Example |
|---|---|---|
| Trait method: `fn severity(&self)` | What the **method body** can do with the struct's data | `&self` = read only, `&mut self` = can modify |
| Function param: `target: &impl Investigate` | What the **caller** gives up | `&` = borrow, `&mut` = exclusive borrow, owned = gone |

### Design impact: `&self` vs `&mut self` in trait contracts

This is where traits diverge most from C# interfaces. When you define a trait method as `&self` or `&mut self`, that choice ripples through your whole architecture:

```rust
// Contract A: agents are read-only — easy to share across threads
trait Agent {
    fn investigate(&self, alert: &Alert) -> Report;
}

// Contract B: agents can mutate internal state — needs exclusive access
trait Agent {
    fn investigate(&mut self, alert: &Alert) -> Report;
}
```

Contract A lets you wrap the agent in `Arc` and share it across async tasks. Contract B requires `Mutex` or exclusive ownership — harder to use concurrently.

### How this shows up in our codebase

Our `SreAgent` trait sidesteps this tension entirely by using async channels (`mpsc`). Each agent owns its own state internally and communicates through messages — so the trait methods don't need `&mut self` even though agents track internal state:

```rust
// From crates/agents/src/traits.rs
pub trait SreAgent: Send + Sync {
    fn id(&self) -> &AgentId;
    fn agent_type(&self) -> AgentType;
    fn capabilities(&self) -> Vec<String>;
    async fn run(&self, inbox: mpsc::Receiver<InboundMessage>) -> Result<(), AgentError>;
}
```

The orchestrator doesn't call methods on the agent directly — it sends messages through channels. This is a deliberate design choice to avoid the `&self` vs `&mut self` problem at the trait level. The `Send + Sync` bound is covered in section 7.

## 7. `Send` and `Sync` — Compile-Time Thread Safety

In C#, any object can be passed to any thread. If it's not thread-safe, you find out at runtime with a race condition or crash. Rust makes the compiler **enforce** thread safety at compile time using two marker traits.

### What are marker traits?

Marker traits are traits with **no methods**. They don't add behavior — they make a promise to the compiler:

| Trait | Promise | C# analogy |
|---|---|---|
| `Send` | This type can be **moved** to another thread | No equivalent — C# objects can always move between threads |
| `Sync` | This type can be **shared** (`&T`) across multiple threads simultaneously | Roughly like saying "this type is thread-safe for shared reads" |

### Trait bounds — the `:` and `+` syntax

When you see `SreAgent: Send + Sync`, the `:` means "requires" and `+` means "and":

```rust
// Any type implementing SreAgent must ALSO be Send AND Sync
pub trait SreAgent: Send + Sync { ... }

// You can stack as many as you need
trait Worker: Send + Sync + 'static { ... }  // Send AND Sync AND owns all its data
```

```csharp
// C# equivalent — multiple interface inheritance
public interface ISreAgent : ISend, ISync { }
// Except Send and Sync don't exist in C# — the runtime handles this implicitly
```

### Why our codebase needs `Send + Sync`

Agents run as **tokio async tasks** that can execute on any thread in the pool:

```rust
// Spawning an agent on a background task
tokio::spawn(async move {
    agent.run(inbox).await
});
```

`tokio::spawn` requires the future to be `Send` — the runtime might schedule it on any thread. If `SreAgent` didn't require `Send + Sync`, this code would not compile.

### Which types are `Send` and `Sync`?

Most types get these **automatically**. The compiler derives them if all fields qualify:

| Type | Send? | Sync? | Why |
|---|---|---|---|
| `String`, `Vec<T>`, `i32` | Yes | Yes | Owned data, no shared state |
| `Arc<T>` (where T: Send + Sync) | Yes | Yes | Thread-safe reference counting |
| `Mutex<T>` | Yes | Yes | Provides synchronized access |
| `Rc<T>` | **No** | **No** | Not thread-safe (use `Arc` instead) |
| Raw pointers `*mut T` | **No** | **No** | Unsafe, no guarantees |

You almost never implement `Send`/`Sync` manually. If all your struct's fields are `Send + Sync`, the compiler auto-derives them. Our agents use `String`, `Arc`, `PlatformConfig` — all `Send + Sync` — so the compiler is happy.

### When you'd get a compile error

```rust
use std::rc::Rc;  // Rc is NOT thread-safe

struct BadAgent {
    cache: Rc<Vec<String>>,  // Rc is not Send or Sync
}

impl SreAgent for BadAgent { ... }
// COMPILE ERROR: `Rc<Vec<String>>` cannot be sent between threads safely
// Fix: use Arc<Vec<String>> instead
```

### C# comparison: runtime vs compile-time safety

```csharp
// C# — compiles fine, blows up at 3 AM in production
var unsafeDict = new Dictionary<string, int>();
Task.Run(() => unsafeDict["a"] = 1);  // race condition — no compiler warning
Task.Run(() => unsafeDict["b"] = 2);  // intermittent data corruption
```

```rust
// Rust — won't compile, caught before code ever runs
use std::collections::HashMap;
let mut map = HashMap::new();
tokio::spawn(async move { map.insert("a", 1); });
// COMPILE ERROR: cannot move out of `map` because it is borrowed
// Fix: wrap in Arc<Mutex<HashMap<...>>>
```

The compiler acts as a **static analysis gate** for concurrency safety. If someone adds a non-thread-safe field to any agent struct, the build fails — not the on-call page.

### `Send + Sync` vs `async` — independent concepts

A common misconception is that `Send + Sync` requires `async fn` or vice versa. They solve **different problems** and are fully independent:

| Concept | What it's about | Needs the other? |
|---|---|---|
| `Send + Sync` | **Thread safety** — can this type cross thread boundaries? | No — works in sync code too |
| `async fn` | **Async execution** — can this function yield and resume? | No — doesn't require Send + Sync |

**`Send` without async — regular OS threads:**

```rust
use std::thread;

// Pure sync code, no async anywhere
fn process_on_thread(data: impl Send + 'static) {
    thread::spawn(move || {
        println!("Processing on another thread");
        // uses data here
    });
}
```

`Send` existed long before `async` was added to Rust. Any time you use `std::thread::spawn`, you need `Send`.

**`async` without `Send` — single-threaded runtime:**

```rust
use std::rc::Rc;

// Single-threaded async runtime — no Send needed
#[tokio::main(flavor = "current_thread")]
async fn main() {
    // Everything runs on ONE thread, so Send is irrelevant
    let non_send_data = Rc::new(42);  // Rc is not Send — fine here
    do_work(non_send_data).await;
}
```

**Why they appear together in our codebase:**

Tokio's **multi-threaded** runtime moves tasks between threads. That creates a dependency chain:

```
tokio::spawn(future)
    → future might run on ANY thread in the pool
    → so the future must be Send
    → so everything the future captures must be Send
    → so SreAgent requires Send + Sync
```

If we used a single-threaded runtime (`tokio::spawn_local`), we wouldn't need `Send + Sync` — but we'd lose multi-core parallelism, which is critical for running multiple investigations concurrently.

**Cheat sheet — when you need what:**

| Scenario | Need Send + Sync? | Need async? |
|---|---|---|
| `std::thread::spawn` (OS threads) | Yes (`Send`) | No |
| `tokio::spawn` (multi-threaded runtime) | Yes (`Send`) | Yes |
| `tokio::spawn_local` (single-threaded runtime) | No | Yes |
| Sequential sync code | No | No |

Think of them as two axes: **async** = "does this code wait for I/O without blocking?" and **Send + Sync** = "does this code work across multiple threads?" Our agents need both because they do I/O (querying Azure APIs) and run concurrently on a thread pool (multiple agents in parallel).

## 8. `Cargo.toml` — Project Configuration

This is your `.csproj` + `NuGet.config` + `.sln` combined:

```toml
[package]                        # Like <PropertyGroup> in .csproj
name = "sre-agents-platform"
version = "0.1.0"
edition = "2021"

[lib]                            # "This crate has a library" (lib.rs)
name = "sre_agents_platform"

[[bin]]                          # "This crate has a binary" (main.rs)
name = "sre-agents"

[dependencies]                   # Like <PackageReference> items
tokio = { version = "1", features = ["full"] }    # NuGet: Install-Package tokio
sre-common = { path = "crates/common" }           # Project reference

[dev-dependencies]               # Like test-only packages
reqwest = "0.13"                 # Only available in tests

[workspace]                      # Like a .sln file
members = ["crates/common", "crates/store", ...]

[profile.release]                # Like Release build config in .csproj
lto = "thin"                     # Link-Time Optimization
strip = true                     # Strip debug symbols
```

## 9. CLI Commands — cargo vs dotnet

| cargo | dotnet | What it does |
|-------|--------|--------------|
| `cargo build` | `dotnet build` | Compile the project |
| `cargo test` | `dotnet test` | Run all tests |
| `cargo run` | `dotnet run` | Build and run the binary |
| `cargo clippy` | `dotnet analyzers` | Lint / static analysis |
| `cargo fmt` | `dotnet format` | Auto-format code |
| `cargo add tokio` | `dotnet add package` | Add a dependency |
| `cargo build --release` | `dotnet build -c Release` | Optimized build |
| `cargo test -p sre-store` | `dotnet test MyProject` | Test a single crate/project |

## 10. The Big Mental Model Shift

Rust makes you **choose** ownership and borrowing (`self`, `&self`, `&mut self`) where C# hides it behind garbage collection. That's the trade — you think harder upfront, but you get:

- **Zero-cost concurrency safety** — no `lock`, no race conditions at runtime
- **No garbage collector** — deterministic cleanup, no GC pauses
- **Compile-time guarantees** — if it compiles, whole classes of bugs are impossible

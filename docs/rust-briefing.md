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
impl WorkflowStore for SqliteStore {
    async fn create_workflow(&self, ...) { ... }
}
```

```csharp
// C# equivalent
public class SqliteStore : IWorkflowStore {
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

### Why this matters in our codebase

We changed `Orchestrator::investigate` from `&mut self` to `&self`. With `&mut self`, Rust guarantees only ONE caller can use the orchestrator at a time (like a C# `lock`). We couldn't `Arc::new()` and share it across tasks. With `&self`, multiple tasks can call `investigate()` concurrently — because they're all just reading shared state (the `Arc`-wrapped store, router, etc. handle their own internal synchronization).

## 6. `Cargo.toml` — Project Configuration

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
reqwest = "0.12"                 # Only available in tests

[workspace]                      # Like a .sln file
members = ["crates/common", "crates/store", ...]

[profile.release]                # Like Release build config in .csproj
lto = "thin"                     # Link-Time Optimization
strip = true                     # Strip debug symbols
```

## 7. CLI Commands — cargo vs dotnet

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

## 8. The Big Mental Model Shift

Rust makes you **choose** ownership and borrowing (`self`, `&self`, `&mut self`) where C# hides it behind garbage collection. That's the trade — you think harder upfront, but you get:

- **Zero-cost concurrency safety** — no `lock`, no race conditions at runtime
- **No garbage collector** — deterministic cleanup, no GC pauses
- **Compile-time guarantees** — if it compiles, whole classes of bugs are impossible

//! Propagating this crate's thread-local context onto spawned workers.
//!
//! Several per-invocation settings live in `thread_local!` storage — the
//! test-only state-dir override ([`crate::paths`]) and the CLI run options
//! ([`crate::runopts`]). A freshly spawned `std::thread` or a `rayon` worker
//! does **not** inherit its parent's TLS, so work moved onto a worker reads the
//! *default* values: the real state dir instead of a test's tempdir, and empty
//! options instead of `--noresync`/`--noconfirm`. That is a genuine bug — it
//! caused a test-isolation flake where a worker escaped a test's sandbox onto
//! the shared real state dir.
//!
//! This module closes that gap and makes it hard to reopen:
//!
//! * Thread-locals are declared with [`context_local!`], **not** bare
//!   `thread_local!` (clippy `disallowed_macros` enforces this). Each
//!   `context_local!` self-registers a propagator into [`PROPAGATORS`] at link
//!   time, so [`Context::capture`] carries it automatically — there is no
//!   central list to forget to update.
//! * Worker-spawning primitives are the [`spawn`] / [`scope`] / [`join`] /
//!   [`thread_pool`] wrappers here, **not** `std::thread::{spawn,scope}` /
//!   `rayon::{join,ThreadPoolBuilder}` (clippy `disallowed_methods` enforces
//!   this). Each wrapper captures the caller's [`Context`] and re-installs it on
//!   the worker.
//!
//! Content-independent by construction: [`Context`] holds opaque installers, so
//! a new [`context_local!`] anywhere is propagated without touching this file.

use std::any::Any;

/// A captured, re-installable snapshot of one thread-local.
///
/// Called on a worker thread, it installs the parent's value and returns a guard
/// whose `Drop` restores the worker's previous value. `Fn` (not `FnOnce`) so a
/// pool's `start_handler` can install onto every worker; `Send + Sync` so it can
/// ride into `rayon`'s pool. Type-erased so [`PROPAGATORS`] is heterogeneous.
pub type Reinstaller = Box<dyn Fn() -> Box<dyn Any> + Send + Sync>;

/// The registry every [`context_local!`] contributes to (via `linkme`, at link
/// time). [`Context::capture`] runs each entry on the calling thread to
/// snapshot that thread-local.
// linkme places the slice in a custom `link_section`, which the compiler treats
// as unsafe; the crate otherwise denies unsafe code.
#[allow(unsafe_code)]
#[linkme::distributed_slice]
pub static PROPAGATORS: [fn() -> Reinstaller];

/// Build a [`Reinstaller`] from a captured `snapshot` and the thread-local's
/// `replace` op.
///
/// `replace(x)` installs `x` and returns the prior value; the guard restores
/// that prior value on drop. Called by the [`context_local!`] expansion; not
/// meant for direct use.
pub fn reinstaller<T, F>(snapshot: T, replace: F) -> Reinstaller
where
    T: Clone + Send + Sync + 'static,
    F: Fn(T) -> T + Copy + Send + Sync + 'static,
{
    Box::new(move || {
        let previous = replace(snapshot.clone());
        Box::new(RestoreOnDrop {
            previous: Some(previous),
            replace,
        }) as Box<dyn Any>
    })
}

/// Guard returned by a [`Reinstaller`]: restores the worker's previous value on
/// drop, so a pooled (`rayon`) worker isn't left carrying one operation's
/// context into the next.
struct RestoreOnDrop<T, F: Fn(T) -> T> {
    previous: Option<T>,
    replace: F,
}

impl<T, F: Fn(T) -> T> Drop for RestoreOnDrop<T, F> {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            (self.replace)(previous);
        }
    }
}

/// A snapshot of the calling thread's propagatable thread-locals.
///
/// Re-installed on a worker so it sees the same context as the thread that
/// spawned it. Holds opaque [`Reinstaller`]s, not named fields — a new
/// [`context_local!`] is carried with no change here.
#[must_use = "a captured Context does nothing until installed on a worker"]
pub struct Context(Vec<Reinstaller>);

impl Context {
    /// Snapshot every registered thread-local on the calling thread.
    pub fn capture() -> Self {
        Self(PROPAGATORS.iter().map(|capture| capture()).collect())
    }

    /// Wrap `f` so that, on whatever thread runs it, this context is installed
    /// for the duration of the call and the worker's prior values restored
    /// afterward (so a reused pool worker isn't contaminated). Used by
    /// [`spawn`] / [`scope`] / [`join`].
    pub fn wrap<F, R>(self, f: F) -> impl FnOnce() -> R
    where
        F: FnOnce() -> R,
    {
        move || {
            let _guards: Vec<Box<dyn Any>> = self.0.iter().map(|install| install()).collect();
            f()
        }
    }

    /// Install this context on the current thread and **keep** it (the restore
    /// guard is intentionally leaked). Only for a worker owned by a *local*,
    /// short-lived pool ([`thread_pool`]) that is dropped after use, so the
    /// worker — and the leak — die with it. Never call on the shared global
    /// pool.
    fn install_for_pool_worker(&self) {
        for install in &self.0 {
            // Leak the guard: the worker keeps the context for its whole life,
            // and dies with the local pool. `Box::leak` (not `mem::forget`,
            // which clippy bans) makes the intent explicit.
            Box::leak(install());
        }
    }
}

/// Capture the calling thread's context and wrap `f` so a spawned/`rayon`
/// worker inherits it. Shorthand for `Context::capture().wrap(f)`.
pub fn propagate<F, R>(f: F) -> impl FnOnce() -> R
where
    F: FnOnce() -> R,
{
    Context::capture().wrap(f)
}

/// `std::thread::spawn`, but the new thread inherits the caller's [`Context`].
/// Prefer over the raw call (clippy `disallowed_methods`).
pub fn spawn<F, T>(f: F) -> std::thread::JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    #[allow(clippy::disallowed_methods)]
    std::thread::spawn(propagate(f))
}

/// `std::thread::scope`, but spawns through a [`Scope`] wrapper whose
/// [`Scope::spawn`] propagates the caller's [`Context`]. Prefer over the raw
/// call (clippy `disallowed_methods`).
pub fn scope<'env, F, T>(f: F) -> T
where
    F: for<'scope> FnOnce(&Scope<'scope, 'env>) -> T,
{
    #[allow(clippy::disallowed_methods)]
    std::thread::scope(|s| f(&Scope(s)))
}

/// A [`std::thread::Scope`] wrapper whose [`spawn`](Self::spawn) carries the
/// caller's [`Context`] onto each scoped thread.
pub struct Scope<'scope, 'env: 'scope>(&'scope std::thread::Scope<'scope, 'env>);

// `'env` is used only in the self type here (the methods borrow for `'scope`),
// so it elides to `'_`; the struct still carries both, mirroring std's Scope.
impl<'scope> Scope<'scope, '_> {
    /// Spawn a scoped thread that inherits the caller's [`Context`].
    pub fn spawn<F, T>(&self, f: F) -> std::thread::ScopedJoinHandle<'scope, T>
    where
        F: FnOnce() -> T + Send + 'scope,
        T: Send + 'scope,
    {
        #[allow(clippy::disallowed_methods)]
        self.0.spawn(propagate(f))
    }
}

/// `rayon::join`, but both closures inherit the caller's [`Context`] (rayon may
/// steal the second onto a worker thread). Prefer over the raw call (clippy
/// `disallowed_methods`).
pub fn join<A, B, RA, RB>(a: A, b: B) -> (RA, RB)
where
    A: FnOnce() -> RA + Send,
    B: FnOnce() -> RB + Send,
    RA: Send,
    RB: Send,
{
    #[allow(clippy::disallowed_methods)]
    rayon::join(propagate(a), propagate(b))
}

/// A local `rayon::ThreadPool` whose workers inherit the caller's [`Context`].
///
/// Prefer over `rayon::ThreadPoolBuilder` (clippy `disallowed_methods`) so
/// `par_iter` bodies run with the caller's thread-locals. The pool must stay
/// local (dropped after use): its workers keep the context for their whole life
/// with no restore, so sharing it would leak one operation's context.
pub fn thread_pool(num_threads: usize) -> Result<rayon::ThreadPool, rayon::ThreadPoolBuildError> {
    let ctx = Context::capture();
    #[allow(clippy::disallowed_methods, clippy::disallowed_types)]
    rayon::ThreadPoolBuilder::new()
        .num_threads(num_threads)
        .start_handler(move |_| ctx.install_for_pool_worker())
        .build()
}

/// Declare a thread-local that propagates onto spawned/`rayon` workers.
///
/// Expands to a module `$name` exposing `get`/`set`/`replace`/`with` over a
/// `thread_local!` cell, **and** a `linkme` registration so the value is
/// captured by [`Context`]. Use this instead of bare `thread_local!`
/// (clippy `disallowed_macros` enforces it) — the registration can't be
/// forgotten because it's part of the same expansion. `$ty` must be
/// `Clone + Send + Sync + 'static`.
#[macro_export]
macro_rules! context_local {
    ($(#[$meta:meta])* $vis:vis static $name:ident: $ty:ty = $init:expr $(;)?) => {
        $(#[$meta])*
        // `redundant_pub_crate`: the accessors stay `pub(crate)` even when
        // `$vis` leaves the module private, since some callers are crate-wide
        // (e.g. `ScopedStateRoot`).
        #[allow(clippy::redundant_pub_crate)]
        $vis mod $name {
            #[allow(unused_imports)]
            use super::*;

            thread_local! {
                static STORE: ::std::cell::RefCell<$ty> = ::std::cell::RefCell::new($init);
            }

            /// Clone of the current thread's value.
            pub(crate) fn get() -> $ty {
                STORE.with(|c| c.borrow().clone())
            }

            /// Install `value`, returning the previous value.
            pub(crate) fn replace(value: $ty) -> $ty {
                STORE.with(|c| ::std::cell::RefCell::replace(c, value))
            }

            /// Overwrite the current thread's value.
            #[allow(dead_code)]
            pub(crate) fn set(value: $ty) {
                STORE.with(|c| *c.borrow_mut() = value);
            }

            /// Borrow the current thread's value for `f`.
            #[allow(dead_code)]
            pub(crate) fn with<R>(f: impl ::std::ops::FnOnce(&$ty) -> R) -> R {
                STORE.with(|c| f(&c.borrow()))
            }

            // linkme uses an unsafe `link_section`; the crate denies unsafe.
            #[allow(unsafe_code)]
            #[::linkme::distributed_slice($crate::context::PROPAGATORS)]
            static PROPAGATOR: fn() -> $crate::context::Reinstaller =
                || $crate::context::reinstaller(get(), replace);
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    // A propagatable thread-local declared only for these tests.
    context_local! {
        static probe: u32 = 0;
    }

    /// A bare spawned thread starts from the default; `propagate` carries the
    /// caller's snapshot across the boundary and restores on the way out.
    // Intentionally uses the raw `std::thread::spawn` as the negative control
    // (the very escape `context::spawn` prevents), so the ban is allowed here.
    #[allow(clippy::disallowed_methods)]
    #[test]
    fn propagate_carries_a_context_local_into_a_spawned_thread() {
        probe::set(7);

        // Without propagation the worker sees the default.
        let bare = std::thread::spawn(probe::get).join().unwrap();
        assert_eq!(bare, 0, "a bare spawned thread sees the default");

        // With propagation it sees the caller's value.
        let carried = std::thread::spawn(propagate(probe::get)).join().unwrap();
        assert_eq!(carried, 7, "propagate carries the value across");

        probe::set(0);
    }

    /// `wrap` restores the worker's previous value afterward, so a *reused*
    /// (rayon) worker isn't contaminated by the task it just ran.
    #[test]
    fn wrap_restores_the_workers_previous_value() {
        probe::set(3);
        let ctx = Context::capture(); // snapshot = 3
        probe::set(99); // now stand in for a reused worker holding a stale value
        let seen = ctx.wrap(probe::get)();
        assert_eq!(seen, 3, "wrap installed the captured value for the call");
        assert_eq!(probe::get(), 99, "wrap restored the worker's prior value");
        probe::set(0);
    }

    /// The `context::scope` + `context::join` wrappers propagate too — a worker
    /// they run reads the caller's value, not the default. Deterministic: a
    /// fresh worker's TLS is *always* the default, so this can't flake.
    #[test]
    fn scope_and_join_wrappers_propagate() {
        probe::set(5);
        let scoped = scope(|s| s.spawn(probe::get).join().unwrap());
        assert_eq!(
            scoped, 5,
            "context::scope carries the value onto the thread"
        );

        let (a, b) = join(probe::get, probe::get);
        assert_eq!(
            (a, b),
            (5, 5),
            "context::join carries the value onto workers"
        );
        probe::set(0);
    }

    /// The other half of the guarantee: TLS must be declared via
    /// `context_local!` (which propagates), never a bare `thread_local!`.
    /// `context.rs` (this wrapper's home) is the sole exception. Enforced here
    /// rather than via clippy `disallowed_macros`, whose lint level resolves at
    /// the macro call site and would force weakening allows onto the very files
    /// that declare TLS.
    #[test]
    fn no_bare_thread_local_outside_context() {
        fn scan(dir: &std::path::Path, offenders: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    scan(&path, offenders);
                } else if path.extension().is_some_and(|e| e == "rs")
                    && path.file_name().is_some_and(|n| n != "context.rs")
                    && std::fs::read_to_string(&path)
                        .unwrap()
                        .contains("thread_local!")
                {
                    offenders.push(path.display().to_string());
                }
            }
        }
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        scan(&src, &mut offenders);
        assert!(
            offenders.is_empty(),
            "declare thread-locals via `context_local!` (src/context.rs), not a \
             bare `thread_local!`, so they propagate onto spawned/rayon workers. \
             Offenders: {offenders:?}",
        );
    }
}

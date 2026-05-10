use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use agent_gateway::agent::{AgentRegistry, AgentSession, AgentDelta};

static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("agent-gateway-test-{n}"));
        std::fs::create_dir_all(&dir).ok();
        TestDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Poll `query_delta` every 1s until `done`, printing and accumulating output.
async fn collect_response(session: &mut dyn AgentSession) -> AgentDelta {
    let mut full = AgentDelta {
        output: String::new(),
        done: false,
    };
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let delta = session.query_delta().unwrap();
        if !delta.output.is_empty() {
            print!("{}", delta.output);
            if !full.output.is_empty() {
                full.output.push('\n');
            }
            full.output.push_str(&delta.output);
        }
        if delta.done {
            full.done = true;
            break;
        }
    }
    full
}

// ---- test cases ----

#[tokio::test]
async fn test_single_turn() {
    let _test_dir = TestDir::new();
    let mut cc = AgentRegistry::new(_test_dir.path().to_path_buf());
    cc.start().await.unwrap();

    let (session_id, mut session) = cc.create_session(std::env::temp_dir()).await.unwrap();
    println!("Session: {session_id}");
    println!("--- test_single_turn ---");
    println!(">>> What is 2+2?");

    session.send_input("What is 2+2?").unwrap();
    let delta = collect_response(&mut *session).await;

    println!(); // trailing newline
    assert!(!delta.output.is_empty(), "expected a response");
    assert!(delta.done, "expected response to be complete");

    cc.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_multi_turn_memory() {
    let _test_dir = TestDir::new();
    let mut cc = AgentRegistry::new(_test_dir.path().to_path_buf());
    cc.start().await.unwrap();

    let (_session_id, mut session) = cc.create_session(std::env::temp_dir()).await.unwrap();

    // Turn 1: introduce a name
    println!("\n--- test_multi_turn_memory ---");
    println!(">>> My name is Alice, remember that.");
    session.send_input("My name is Alice, remember that.").unwrap();
    let d1 = collect_response(&mut *session).await;
    println!();
    assert!(!d1.output.is_empty());

    // Turn 2: ask for the name
    println!(">>> What is my name?");
    session.send_input("What is my name?").unwrap();
    let d2 = collect_response(&mut *session).await;
    println!();

    assert!(!d2.output.is_empty());
    assert!(
        d2.output.to_lowercase().contains("alice"),
        "expected response to contain 'Alice', got: {}",
        d2.output,
    );

    cc.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_joke() {
    let _test_dir = TestDir::new();
    let mut cc = AgentRegistry::new(_test_dir.path().to_path_buf());
    cc.start().await.unwrap();

    let (_session_id, mut session) = cc.create_session(std::env::temp_dir()).await.unwrap();

    println!("\n--- test_joke ---");
    println!(">>> Tell me a short joke.");
    session.send_input("Tell me a short joke.").unwrap();
    let delta = collect_response(&mut *session).await;
    println!();

    assert!(!delta.output.is_empty(), "expected a joke response");

    cc.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_tool_read_cargo_toml() {
    let _test_dir = TestDir::new();
    let mut cc = AgentRegistry::new(_test_dir.path().to_path_buf());
    cc.start().await.unwrap();

    let pwd = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
    let (_session_id, mut session) = cc.create_session(pwd).await.unwrap();

    println!("\n--- test_tool_read_cargo_toml ---");
    println!(">>> Read the file Cargo.toml and list all dependencies.");
    session.send_input("Read the file Cargo.toml and list all dependencies with their versions.")
        .unwrap();
    let delta = collect_response(&mut *session).await;
    println!();

    assert!(!delta.output.is_empty(), "expected a response");
    assert!(
        delta.output.contains("matrix-sdk") || delta.output.contains("tokio"),
        "expected response to mention matrix-sdk or tokio from Cargo.toml, got: {}",
        delta.output,
    );

    cc.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_session_persistence() {
    let _test_dir = TestDir::new();
    let mut cc = AgentRegistry::new(_test_dir.path().to_path_buf());
    cc.start().await.unwrap();

    let (session_id, mut session) = cc.create_session(std::env::temp_dir()).await.unwrap();

    // Turn 1
    println!("\n--- test_session_persistence ---");
    println!(">>> I like pizza.");
    session.send_input("I like pizza.").unwrap();
    let _ = collect_response(&mut *session).await;
    println!();
    println!("Session: {session_id}");

    // Turn 2
    println!(">>> What food did I say I like?");
    session.send_input("What food did I say I like?").unwrap();
    let d2 = collect_response(&mut *session).await;
    println!();

    assert!(
        d2.output.to_lowercase().contains("pizza"),
        "expected response to mention pizza, got: {}",
        d2.output,
    );

    cc.shutdown().await.unwrap();
}

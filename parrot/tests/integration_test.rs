use std::process::Command;
use std::time::Duration;

#[test]
fn run_all_tests() {
    // Test files to run
    let test_files = vec![
        "test_actor_basics.rs",
        "test_actor_macro.rs",
        "test_async_message.rs",
        "test_sync_message.rs",
        "test_system.rs"
    ];
    
    // Run each test file as an individual process
    for test_file in test_files {
        println!("Running test file: {}", test_file);
        
        // Use cargo to run the test file
        let status = Command::new("cargo")
            .args(["run", "--bin", &test_file.replace(".rs", "")])
            .status()
            .expect("Failed to execute test");
            
        assert!(status.success(), "Test {} failed with status: {}", test_file, status);
    }
} 

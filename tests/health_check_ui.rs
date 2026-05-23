#[test]
fn health_check_compile_errors() {
    let tests = trybuild::TestCases::new();
    tests.compile_fail("tests/ui/health_check_*.rs");
}

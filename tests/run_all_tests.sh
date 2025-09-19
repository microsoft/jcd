#!/bin/bash

echo "=== JCD Comprehensive Test Suite ==="
echo "Running all tests to verify functionality..."
echo

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

TESTS_PASSED=0
TESTS_FAILED=0
FAILED_TESTS=()

run_test() {
    local test_name="$1"
    local test_script="$2"

    echo -e "${YELLOW}Running: $test_name${NC}"
    echo "----------------------------------------"

    if [ -f "$test_script" ] && [ -x "$test_script" ]; then
        if "$test_script"; then
            echo -e "${GREEN}✓ PASSED: $test_name${NC}"
            ((TESTS_PASSED++))
        else
            echo -e "${RED}✗ FAILED: $test_name${NC}"
            ((TESTS_FAILED++))
            FAILED_TESTS+=("$test_name")
        fi
    else
        echo -e "${RED}✗ SKIPPED: $test_name (script not found or not executable)${NC}"
        ((TESTS_FAILED++))
        FAILED_TESTS+=("$test_name (not executable)")
    fi
    echo
}

run_python_test() {
    local test_name="$1"
    local test_script="$2"

    echo -e "${YELLOW}Running: $test_name${NC}"
    echo "----------------------------------------"

    if [ -f "$test_script" ]; then
        if python3 "$test_script"; then
            echo -e "${GREEN}✓ PASSED: $test_name${NC}"
            ((TESTS_PASSED++))
        else
            echo -e "${RED}✗ FAILED: $test_name${NC}"
            ((TESTS_FAILED++))
            FAILED_TESTS+=("$test_name")
        fi
    else
        echo -e "${RED}✗ SKIPPED: $test_name (script not found)${NC}"
        ((TESTS_FAILED++))
        FAILED_TESTS+=("$test_name (not found)")
    fi
    echo
}

# Change to the tests directory
cd "$(dirname "$0")"

# Make sure all shell scripts are executable
chmod +x *.sh 2>/dev/null

echo "Starting test execution..."
echo

# Core functionality tests
run_test "Simple Functionality Test" "./simple_test.sh"
run_test "Comprehensive Relative Path Test" "./test_relative_comprehensive.sh"
run_test "Ignore Functionality Test" "./test_ignore_functionality.sh"
run_test "Validation Test" "./validate_jcd.sh"

# Regression and bug fix tests
run_test "Quick Regression Test" "./quick_regression_test.sh"
run_test "Absolute Bug Test" "./test_absolute_bug.sh"
run_test "Absolute Path Consistency Test" "./test_absolute_path_consistency.sh"
run_test "Regression Fix Test" "./test_regression_fix.sh"
run_test "Final Absolute Path Test" "./final_absolute_path_test.sh"
run_test "Symlink Fix Test" "./test_symlink.sh"

# Python-based tests
run_python_test "Basic Functionality Verification (Python)" "./verify_basic_functionality.py"

# Summary
echo "========================================"
echo "TEST SUITE SUMMARY"
echo "========================================"
echo -e "Total Tests: $((TESTS_PASSED + TESTS_FAILED))"
echo -e "${GREEN}Passed: $TESTS_PASSED${NC}"
echo -e "${RED}Failed: $TESTS_FAILED${NC}"

if [ $TESTS_FAILED -eq 0 ]; then
    echo -e "${GREEN}🎉 ALL TESTS PASSED!${NC}"
    exit 0
else
    echo
    echo -e "${RED}Failed Tests:${NC}"
    for test in "${FAILED_TESTS[@]}"; do
        echo -e "${RED}  - $test${NC}"
    done
    echo
    echo -e "${RED}❌ SOME TESTS FAILED${NC}"
    exit 1
fi
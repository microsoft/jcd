#!/bin/bash

# Test script to verify symlink handling fix
echo "=== JCD Symlink Fix Test ==="

# Create test structure
TEST_DIR="/tmp/jcd_symlink_test"
ORIG_DIR="$TEST_DIR/original"
SYMLINK_DIR="$TEST_DIR/symlink"

echo "Setting up test environment..."
rm -rf "$TEST_DIR"
mkdir -p "$ORIG_DIR/subdir1/nested"
mkdir -p "$ORIG_DIR/subdir2"
mkdir -p "$ORIG_DIR/markrussinovich"

# Create symlink
ln -sf "$ORIG_DIR" "$SYMLINK_DIR"

JCD_BINARY="/datadrive/jcd/target/release/jcd"

echo "Test structure:"
echo "  $ORIG_DIR/ (original)"
echo "  $SYMLINK_DIR/ -> $ORIG_DIR (symlink)"
echo ""

# Test 1: Basic subdirectory search from symlink
echo "Test 1: Basic subdirectory search from symlink"
cd "$SYMLINK_DIR"
pwd
RESULT=$($JCD_BINARY markrussinovich)
echo "jcd markrussinovich -> $RESULT"
if [[ "$RESULT" == "$SYMLINK_DIR/markrussinovich" ]]; then
    echo "✓ PASSED - Preserves symlink path"
else
    echo "✗ FAILED - Expected $SYMLINK_DIR/markrussinovich, got $RESULT"
fi
echo ""

# Test 2: Parent directory navigation from symlink subdirectory
echo "Test 2: Parent directory navigation from symlink subdirectory"
cd "$SYMLINK_DIR/markrussinovich"
pwd
RESULT=$($JCD_BINARY ..)
echo "jcd .. -> $RESULT"
if [[ "$RESULT" == "$SYMLINK_DIR" ]]; then
    echo "✓ PASSED - Parent navigation preserves symlink path"
else
    echo "✗ FAILED - Expected $SYMLINK_DIR, got $RESULT"
fi
echo ""

# Test 3: Nested subdirectory search
echo "Test 3: Nested subdirectory search"
cd "$SYMLINK_DIR"
RESULT=$($JCD_BINARY nested)
echo "jcd nested -> $RESULT"
if [[ "$RESULT" == "$SYMLINK_DIR/subdir1/nested" ]]; then
    echo "✓ PASSED - Nested search preserves symlink path"
else
    echo "✗ FAILED - Expected $SYMLINK_DIR/subdir1/nested, got $RESULT"
fi
echo ""

# Test 4: Case insensitive search
echo "Test 4: Case insensitive search"
cd "$SYMLINK_DIR"
RESULT=$($JCD_BINARY -i MARKRUSSINOVICH)
echo "jcd -i MARKRUSSINOVICH -> $RESULT"
if [[ "$RESULT" == "$SYMLINK_DIR/markrussinovich" ]]; then
    echo "✓ PASSED - Case insensitive search preserves symlink path"
else
    echo "✗ FAILED - Expected $SYMLINK_DIR/markrussinovich, got $RESULT"
fi
echo ""

# Test 5: Absolute path should not be affected
echo "Test 5: Absolute path should not be affected"
cd "$SYMLINK_DIR"
RESULT=$($JCD_BINARY /tmp)
echo "jcd /tmp -> $RESULT"
if [[ "$RESULT" == "/tmp" ]]; then
    echo "✓ PASSED - Absolute paths work correctly"
else
    echo "✗ FAILED - Expected /tmp, got $RESULT"
fi
echo ""

# Test 6: Two level parent navigation
echo "Test 6: Two level parent navigation"
cd "$SYMLINK_DIR/subdir1/nested"
pwd
RESULT=$($JCD_BINARY ../..)
echo "jcd ../.. -> $RESULT"
if [[ "$RESULT" == "$SYMLINK_DIR" ]]; then
    echo "✓ PASSED - Multi-level parent navigation preserves symlink path"
else
    echo "✗ FAILED - Expected $SYMLINK_DIR, got $RESULT"
fi
echo ""

# Cleanup
echo "Cleaning up..."
rm -rf "$TEST_DIR"

echo "=== Test complete ==="

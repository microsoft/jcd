use regex::{Regex, RegexBuilder};
use std::{
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

// Configuration constants for performance tuning
const MAX_MATCHES: usize = 20; // Stop after finding enough matches
const MAX_SEARCH_TIME_MS: u64 = 500; // Max time to spend searching (milliseconds)
const MAX_IGNORE_PATTERNS: usize = 100; // Upper bound on loaded ignore patterns
const MAX_COMPILED_REGEX_SIZE: usize = 1_000_000; // 1MB compiled regex size limit

/// Get the current working directory preserving symlinks using PWD environment variable
/// Falls back to env::current_dir() if PWD is not available or invalid
fn get_current_dir_preserving_symlinks() -> Result<PathBuf, std::io::Error> {
    // Try to use PWD environment variable first (preserves symlinks)
    if let Ok(pwd) = env::var("PWD") {
        let pwd_path = PathBuf::from(&pwd);
        
        // Verify that PWD actually points to the same directory as the canonical current dir
        if let Ok(canonical_current) = env::current_dir() {
            if let Ok(canonical_pwd) = pwd_path.canonicalize() {
                if canonical_pwd == canonical_current {
                    return Ok(pwd_path);
                }
            }
        }
    }
    
    // Fall back to the standard method if PWD is not reliable
    env::current_dir()
}

/// Convert a canonical path back to a symlink-preserving path
fn preserve_symlink_in_path(canonical_path: &Path, user_cwd: &Path, canonical_cwd: &Path) -> PathBuf {
    // If the canonical path starts with the canonical current directory,
    // replace that prefix with the user's symlink current directory
    if let Ok(relative_path) = canonical_path.strip_prefix(canonical_cwd) {
        if relative_path.as_os_str().is_empty() {
            // When the relative path is empty (i.e., canonical_path == canonical_cwd),
            // return user_cwd directly to avoid the trailing slash from join("")
            user_cwd.to_path_buf()
        } else {
            user_cwd.join(relative_path)
        }
    } else if let Ok(relative_to_parent) = canonical_cwd.strip_prefix(canonical_path) {
        // Handle parent directories: navigate up from user_cwd
        let levels_up = relative_to_parent.components().count();
        let mut result_path = user_cwd.to_path_buf();
        for _ in 0..levels_up {
            if let Some(parent) = result_path.parent() {
                result_path = parent.to_path_buf();
            } else {
                break;
            }
        }
        result_path
    } else {
        // If it's not related to current directory, return as-is
        canonical_path.to_path_buf()
    }
}

/// Get ignore file paths in priority order following XDG Base Directory Specification
fn get_ignore_file_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    // 1. Project-local ignore file (highest precedence)
    if let Ok(current_dir) = env::current_dir() {
        paths.push(current_dir.join(".jcdignore"));
    }

    // 2. User XDG config directory
    let config_home = env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            env::var("HOME")
                .map(|home| PathBuf::from(home).join(".config"))
                .unwrap_or_else(|_| PathBuf::from(".config"))
        });
    paths.push(config_home.join("jcd").join("ignore"));

    // 3. Legacy dotfile for backward compatibility
    if let Ok(home) = env::var("HOME") {
        paths.push(PathBuf::from(home).join(".jcdignore"));
    }

    // 4. System-wide configuration
    paths.push(PathBuf::from("/etc/jcd/ignore"));

    paths
}

/// Parse ignore patterns from file content
fn parse_ignore_patterns(content: &str) -> Vec<Regex> {
    let mut patterns = Vec::new();

    for line in content.lines() {
        let line = line.trim();

        // Skip empty lines and comments
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Try to compile the regex pattern
        match RegexBuilder::new(line)
            .size_limit(MAX_COMPILED_REGEX_SIZE)
            .build()
        {
            Ok(regex) => {
                if patterns.len() < MAX_IGNORE_PATTERNS {
                    patterns.push(regex);
                } else if is_debug_enabled() {
                    eprintln!(
                        "DEBUG: Ignored pattern due to max pattern count (100): '{}'",
                        line
                    );
                }
            }
            Err(e) => {
                if is_debug_enabled() {
                    eprintln!("DEBUG: Invalid regex pattern '{}': {}", line, e);
                }
                // Continue processing other patterns even if one is invalid
            }
        }
    }

    patterns
}

/// Load ignore patterns from standard locations
fn load_ignore_patterns() -> Vec<Regex> {
    let ignore_files = get_ignore_file_paths();

    for file_path in ignore_files {
        if is_debug_enabled() {
            eprintln!("DEBUG: Checking ignore file: {}", file_path.display());
        }

        if let Ok(content) = fs::read_to_string(&file_path) {
            if is_debug_enabled() {
                eprintln!("DEBUG: Found ignore file: {}", file_path.display());
            }
            let patterns = parse_ignore_patterns(&content);
            if is_debug_enabled() {
                eprintln!("DEBUG: Loaded {} ignore patterns", patterns.len());
            }
            return patterns;
        }
    }

    if is_debug_enabled() {
        eprintln!("DEBUG: No ignore file found");
    }
    Vec::new()
}

/// Check if a directory should be ignored based on patterns
fn should_ignore_directory(dir_name: &str, ignore_patterns: &[Regex]) -> bool {
    ignore_patterns
        .iter()
        .any(|pattern| pattern.is_match(dir_name))
}

fn is_debug_enabled() -> bool {
    env::var("JCD_DEBUG").unwrap_or_default() == "1"
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum MatchQuality {
    ExactUp,     // Exact match up the path - highest priority
    PartialUp,   // Partial match up the path - second priority
    ExactDown,   // Exact match down the path - third priority
    PrefixDown,  // Prefix match down the path - fourth priority
    PartialDown, // Partial match down the path - lowest priority
}

#[derive(Debug, Clone)]
struct DirectoryMatch {
    path: PathBuf,
    depth_from_current: i32, // negative for parents, positive for children
    match_quality: MatchQuality,
}

#[derive(Debug)]
struct SearchContext {
    start_time: Instant,
    max_matches: usize,
    max_time: Duration,
    current_matches: usize,
}

impl SearchContext {
    fn new() -> Self {
        Self {
            start_time: Instant::now(),
            max_matches: MAX_MATCHES,
            max_time: Duration::from_millis(MAX_SEARCH_TIME_MS),
            current_matches: 0,
        }
    }

    fn should_continue(&self) -> bool {
        self.current_matches < self.max_matches && self.start_time.elapsed() < self.max_time
    }

    fn add_match(&mut self) {
        self.current_matches += 1;
    }
}

/// Resolves the search context by handling relative paths and directory navigation patterns.
/// Returns (search_directory, pattern) where search_directory is the resolved starting point
/// and pattern is the remaining search term after resolving relative components.
fn resolve_search_context(canonical_current_dir: &Path, search_term: &str, user_current_dir: &Path) -> (PathBuf, String) {
    if is_debug_enabled() {
        eprintln!(
            "DEBUG: resolve_search_context: canonical_current_dir={}, user_current_dir={}, search_term='{}'",
            canonical_current_dir.display(),
            user_current_dir.display(),
            search_term
        );
    }

    // Handle empty search term
    if search_term.is_empty() {
        return (canonical_current_dir.to_path_buf(), String::new());
    }

    // Handle simple directory navigation
    if search_term == ".." {
        return resolve_parent_directory(user_current_dir, canonical_current_dir);
    }

    if search_term == "." {
        return (canonical_current_dir.to_path_buf(), String::new());
    }

    // Handle relative paths with patterns like "../foo", "../../bar", etc.
    if search_term.starts_with("../") || search_term.starts_with("./") {
        return resolve_relative_path(search_term, user_current_dir);
    }

    // Handle multiple levels of parent directory navigation like "../../", "../../../"
    if search_term.chars().all(|c| c == '.' || c == '/') && search_term.contains("..") {
        return resolve_multi_parent_navigation(search_term, user_current_dir);
    }

    // For absolute paths and regular patterns, use the original behavior
    (canonical_current_dir.to_path_buf(), search_term.to_string())
}

/// Helper function to resolve parent directory navigation
fn resolve_parent_directory(user_current_dir: &Path, canonical_current_dir: &Path) -> (PathBuf, String) {
    if let Some(user_parent) = user_current_dir.parent() {
        if let Ok(canonical_parent) = user_parent.canonicalize() {
            return (canonical_parent, String::new());
        }
    }
    // Fallback to canonical navigation
    if let Some(canonical_parent) = canonical_current_dir.parent() {
        (canonical_parent.to_path_buf(), String::new())
    } else {
        (canonical_current_dir.to_path_buf(), String::new())
    }
}

/// Helper function to resolve relative paths with patterns
fn resolve_relative_path(search_term: &str, user_current_dir: &Path) -> (PathBuf, String) {
    let path = Path::new(search_term);
    let mut resolved_user_dir = user_current_dir.to_path_buf();
    let mut remaining_pattern = String::new();

    for component in path.components() {
        match component {
            std::path::Component::CurDir => continue,
            std::path::Component::ParentDir => {
                if let Some(parent) = resolved_user_dir.parent() {
                    resolved_user_dir = parent.to_path_buf();
                }
            }
            std::path::Component::Normal(name) => {
                remaining_pattern = name.to_string_lossy().to_string();
                break;
            }
            _ => break,
        }
    }

    // Convert to canonical for internal operations
    if let Ok(canonical_resolved) = resolved_user_dir.canonicalize() {
        (canonical_resolved, remaining_pattern)
    } else {
        (resolved_user_dir, remaining_pattern)
    }
}

/// Helper function to resolve multiple parent directory navigation
fn resolve_multi_parent_navigation(search_term: &str, user_current_dir: &Path) -> (PathBuf, String) {
    let mut resolved_user_dir = user_current_dir.to_path_buf();
    let path = Path::new(search_term);

    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                if let Some(parent) = resolved_user_dir.parent() {
                    resolved_user_dir = parent.to_path_buf();
                }
            }
            std::path::Component::CurDir => continue,
            _ => break,
        }
    }

    // Convert to canonical for internal operations
    if let Ok(canonical_resolved) = resolved_user_dir.canonicalize() {
        (canonical_resolved, String::new())
    } else {
        (resolved_user_dir, String::new())
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        eprintln!("Error: No search term provided");
        process::exit(1);
    }

    // Parse command line arguments for flags
    let mut case_sensitive = true; // Default to case sensitive
    let mut search_term = String::new();
    let mut tab_index = 0;
    let mut quiet_mode = false;
    let mut bypass_ignore = false; // -x flag to bypass ignore patterns

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-i" => {
                case_sensitive = false; // -i flag makes it case insensitive
                i += 1;
            }
            "-x" => {
                bypass_ignore = true; // -x flag bypasses ignore patterns
                i += 1;
            }
            "--quiet" => {
                quiet_mode = true;
                i += 1;
            }
            arg => {
                if search_term.is_empty() {
                    search_term = arg.to_string();
                } else if tab_index == 0 {
                    tab_index = arg.parse::<usize>().unwrap_or(0);
                }
                i += 1;
            }
        }
    }

    if search_term.is_empty() {
        eprintln!("Error: No search term provided");
        process::exit(1);
    }

    // Get current directory preserving symlinks for user experience
    let user_current_dir = match get_current_dir_preserving_symlinks() {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("Error: Cannot get current directory: {}", e);
            process::exit(1);
        }
    };

    // Also get the canonical current directory for internal filesystem operations
    let canonical_current_dir = match env::current_dir() {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("Error: Cannot get canonical current directory: {}", e);
            process::exit(1);
        }
    };

    // Handle relative paths and standard directory navigation using canonical path
    let (search_dir, pattern) = resolve_search_context(&canonical_current_dir, &search_term, &user_current_dir);

    if is_debug_enabled() {
        eprintln!(
            "DEBUG: Searching for '{}' from {}",
            pattern,
            search_dir.display()
        );
    }

    // Load ignore patterns unless bypassed
    let ignore_patterns = if bypass_ignore {
        if is_debug_enabled() {
            eprintln!("DEBUG: Bypassing ignore patterns (-x flag)");
        }
        Vec::new()
    } else {
        load_ignore_patterns()
    };

    // Use threaded search with busy indicator (unless in quiet mode)
    let matches = if quiet_mode {
        find_matching_directories(&search_dir, &pattern, case_sensitive, &ignore_patterns, &user_current_dir, &canonical_current_dir)
    } else {
        search_with_progress(&search_dir, &pattern, case_sensitive, &ignore_patterns, &user_current_dir, &canonical_current_dir)
    };

    if is_debug_enabled() {
        eprintln!("DEBUG: Found {} matches", matches.len());
    }

    if matches.is_empty() || tab_index >= matches.len() {
        if is_debug_enabled() {
            eprintln!("DEBUG: No matches or index out of range");
        }
        process::exit(1);
    }

    println!("{}", matches[tab_index].path.display());
}

fn search_with_progress(
    current_dir: &Path,
    search_term: &str,
    case_sensitive: bool,
    ignore_patterns: &[Regex],
    user_current_dir: &Path,
    canonical_current_dir: &Path,
) -> Vec<DirectoryMatch> {
    let current_dir = current_dir.to_path_buf();
    let search_term = search_term.to_string();
    let ignore_patterns = ignore_patterns.to_vec(); // Clone for thread
    let user_current_dir = user_current_dir.to_path_buf(); // Clone for thread
    let canonical_current_dir = canonical_current_dir.to_path_buf(); // Clone for thread

    // Shared state for the search result
    let result = Arc::new(Mutex::new(None));
    let result_clone = Arc::clone(&result);

    // Shared flag to indicate when search is complete
    let search_complete = Arc::new(Mutex::new(false));
    let search_complete_clone = Arc::clone(&search_complete);

    // Start the search in a background thread
    let search_handle = thread::spawn(move || {
        let matches =
            find_matching_directories(&current_dir, &search_term, case_sensitive, &ignore_patterns, &user_current_dir, &canonical_current_dir);

        // Store the result
        {
            let mut result_guard = result_clone.lock().unwrap();
            *result_guard = Some(matches);
        }

        // Mark search as complete
        {
            let mut complete_guard = search_complete_clone.lock().unwrap();
            *complete_guard = true;
        }
    });

    // Give search a brief moment to complete (20ms)
    thread::sleep(Duration::from_millis(20));

    // Check if search is still running
    let show_progress = {
        let complete_guard = search_complete.lock().unwrap();
        !*complete_guard
    };

    if show_progress {
        // Start the busy indicator in a separate thread
        let search_complete_clone = Arc::clone(&search_complete);
        let indicator_handle = thread::spawn(move || {
            show_busy_indicator(&search_complete_clone);
        });

        // Wait for the search to complete
        search_handle.join().unwrap();

        // Wait for indicator to finish
        indicator_handle.join().unwrap();

        // Clear the progress line
        eprint!("\r\x1b[K");
        io::stderr().flush().unwrap();
    } else {
        // Search completed quickly, just wait for it
        search_handle.join().unwrap();
    }

    // Return the result
    let result_guard = result.lock().unwrap();
    result_guard.as_ref().unwrap().clone()
}

fn show_busy_indicator(search_complete: &Arc<Mutex<bool>>) {
    let dots = [" .", " ..", " ..."];
    let mut dot_index = 0;

    loop {
        // Check if search is complete
        {
            let complete_guard = search_complete.lock().unwrap();
            if *complete_guard {
                break;
            }
        }

        // Show the dots animation with carriage return
        eprint!("\r{}", dots[dot_index]);
        io::stderr().flush().unwrap();

        // Update dot index
        dot_index = (dot_index + 1) % dots.len();

        // Wait before next update
        thread::sleep(Duration::from_millis(200));
    }
}

fn find_matching_directories(
    current_dir: &Path,
    search_term: &str,
    case_sensitive: bool,
    ignore_patterns: &[Regex],
    user_current_dir: &Path,
    canonical_current_dir: &Path,
) -> Vec<DirectoryMatch> {
    if is_debug_enabled() {
        eprintln!(
            "DEBUG: find_matching_directories: current_dir={}, search_term='{}', case_sensitive={}",
            current_dir.display(),
            search_term,
            case_sensitive
        );
    }

    let mut matches = Vec::new();

    // Handle empty search term (pure directory navigation like "..", "../../")
    if search_term.is_empty() {
        if is_debug_enabled() {
            eprintln!("DEBUG: Empty search term, returning current directory as match");
        }
        matches.push(DirectoryMatch {
            path: preserve_symlink_in_path(current_dir, user_current_dir, canonical_current_dir),
            depth_from_current: 0,
            match_quality: MatchQuality::ExactDown,
        });
        return matches;
    }

    // Handle absolute paths
    if search_term.starts_with('/') {
        if is_debug_enabled() {
            eprintln!("DEBUG: Processing absolute path: {}", search_term);
        }
        let path = Path::new(search_term);

        if search_term.ends_with('/') {
            if is_debug_enabled() {
                eprintln!("DEBUG: Absolute path ends with slash - exploring subdirectories");
            }
            let dir_path = Path::new(&search_term[..search_term.len() - 1]);
            if dir_path.exists() && dir_path.is_dir() {
                let mut subdir_matches = Vec::new();
                search_absolute_pattern(dir_path, "", &mut subdir_matches, case_sensitive);

                if !subdir_matches.is_empty() {
                    if is_debug_enabled() {
                        eprintln!(
                            "DEBUG: Found {} subdirectories in {}",
                            subdir_matches.len(),
                            dir_path.display()
                        );
                    }
                    matches.extend(subdir_matches);
                } else {
                    if is_debug_enabled() {
                        eprintln!("DEBUG: No subdirectories found, returning directory itself");
                    }
                    matches.push(DirectoryMatch {
                        path: dir_path.to_path_buf(),
                        depth_from_current: 0,
                        match_quality: MatchQuality::ExactDown,
                    });
                }
            } else {
                if is_debug_enabled() {
                    eprintln!("DEBUG: Directory doesn't exist, treating as pattern search");
                }
                let search_term_no_slash = &search_term[..search_term.len() - 1];
                let (search_root, search_pattern) =
                    find_search_root_and_pattern(search_term_no_slash);
                if let Some(root) = search_root {
                    search_absolute_pattern(&root, &search_pattern, &mut matches, case_sensitive);
                }
            }
        } else if path.exists() && path.is_dir() {
            if is_debug_enabled() {
                eprintln!("DEBUG: Absolute path exists exactly, returning it directly");
            }
            matches.push(DirectoryMatch {
                path: path.to_path_buf(),
                depth_from_current: 0,
                match_quality: MatchQuality::ExactDown,
            });
        } else {
            if is_debug_enabled() {
                eprintln!("DEBUG: Absolute path doesn't exist, finding search root and pattern");
            }
            let (search_root, search_pattern) = find_search_root_and_pattern(search_term);
            if let Some(root) = search_root {
                if is_debug_enabled() {
                    eprintln!(
                        "DEBUG: Searching from root {} for pattern '{}'",
                        root.display(),
                        search_pattern
                    );
                }
                search_absolute_pattern(&root, &search_pattern, &mut matches, case_sensitive);
            }
        }
        return finalize_matches(matches);
    }

    // Handle path-like patterns (contains '/')
    if search_term.contains('/') {
        if is_debug_enabled() {
            eprintln!("DEBUG: Processing path-like pattern with '/'");
        }
        let mut context = SearchContext::new();
        search_path_pattern_fast(
            current_dir,
            search_term,
            &mut matches,
            &mut context,
            case_sensitive,
            user_current_dir,
            canonical_current_dir,
        );
        if !matches.is_empty() {
            if is_debug_enabled() {
                eprintln!("DEBUG: Found {} matches for path pattern", matches.len());
            }
            return finalize_matches(matches);
        }
    }

    if is_debug_enabled() {
        eprintln!("DEBUG: Starting comprehensive search - up tree then down tree");
    }

    // 1. Search up for exact matches, then partial matches (direct path to root only)
    let up_matches =
        search_up_tree_with_priority(current_dir, search_term, case_sensitive, ignore_patterns, user_current_dir, canonical_current_dir);
    if is_debug_enabled() {
        eprintln!(
            "DEBUG: Found {} matches searching up tree",
            up_matches.len()
        );
    }
    matches.extend(up_matches);

    // 2. Search down for all matches (exact and partial) from current directory only
    let down_matches =
        search_down_breadth_first_all(current_dir, search_term, case_sensitive, ignore_patterns, user_current_dir, canonical_current_dir);
    if is_debug_enabled() {
        eprintln!(
            "DEBUG: Found {} matches searching down tree",
            down_matches.len()
        );
    }
    matches.extend(down_matches);

    // Return all matches sorted by priority
    if !matches.is_empty() {
        if is_debug_enabled() {
            eprintln!("DEBUG: Total {} matches found, finalizing", matches.len());
        }
        return finalize_matches(matches);
    }

    if is_debug_enabled() {
        eprintln!("DEBUG: No matches found");
    }
    Vec::new()
}

fn search_up_tree_with_priority(
    current_dir: &Path,
    search_term: &str,
    case_sensitive: bool,
    ignore_patterns: &[Regex],
    user_current_dir: &Path,
    canonical_current_dir: &Path,
) -> Vec<DirectoryMatch> {
    let mut exact_matches = Vec::new();
    let mut partial_matches = Vec::new();
    let mut current = current_dir;
    let mut depth = -1;

    let search_compare = if case_sensitive {
        search_term.to_string()
    } else {
        search_term.to_lowercase()
    };

    while let Some(parent) = current.parent() {
        if let Some(name) = parent.file_name() {
            let name_str = name.to_string_lossy();

            // Skip ignored directories
            if should_ignore_directory(&name_str, ignore_patterns) {
                current = parent;
                depth -= 1;
                continue;
            }

            let name_compare = if case_sensitive {
                name_str.to_string()
            } else {
                name_str.to_lowercase()
            };

            if name_compare == search_compare {
                exact_matches.push(DirectoryMatch {
                    path: preserve_symlink_in_path(parent, user_current_dir, canonical_current_dir),
                    depth_from_current: depth,
                    match_quality: MatchQuality::ExactUp,
                });
            } else if name_compare.contains(&search_compare) {
                partial_matches.push(DirectoryMatch {
                    path: preserve_symlink_in_path(parent, user_current_dir, canonical_current_dir),
                    depth_from_current: depth,
                    match_quality: MatchQuality::PartialUp,
                });
            }
        }
        current = parent;
        depth -= 1;
    }

    let mut result = exact_matches;
    result.extend(partial_matches);
    result
}

fn search_down_breadth_first_all(
    current_dir: &Path,
    search_term: &str,
    case_sensitive: bool,
    ignore_patterns: &[Regex],
    user_current_dir: &Path,
    canonical_current_dir: &Path,
) -> Vec<DirectoryMatch> {
    if is_debug_enabled() {
        eprintln!(
            "DEBUG: search_down_breadth_first_all: searching for '{}', case_sensitive={}",
            search_term, case_sensitive
        );
    }

    use std::collections::VecDeque;

    let mut queue = VecDeque::new();
    let mut all_matches = Vec::new();
    queue.push_back((current_dir.to_path_buf(), 0));
    let search_lower = if case_sensitive {
        search_term.to_string()
    } else {
        search_term.to_lowercase()
    };
    let max_depth = 8;

    // First, search immediate subdirectories (depth 1) to check for early stopping
    let mut immediate_matches = Vec::new();

    if is_debug_enabled() {
        eprintln!(
            "DEBUG: Processing immediate subdirectories in {}",
            current_dir.display()
        );
    }

    // Process current directory (depth 0) first
    if let Ok(entries) = fs::read_dir(current_dir) {
        let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
        entries.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

        for entry in &entries {
            if let Ok(metadata) = entry.metadata() {
                if metadata.is_dir() {
                    let path = entry.path();
                    if let Some(name) = path.file_name() {
                        let name_str = name.to_string_lossy();

                        // Check if this directory should be ignored
                        if should_ignore_directory(&name_str, ignore_patterns) {
                            if is_debug_enabled() {
                                eprintln!("DEBUG: Ignoring directory: {}", name_str);
                            }
                            continue;
                        }

                        let (name_compare, search_compare) = if case_sensitive {
                            (name_str.to_string(), search_term.to_string())
                        } else {
                            (name_str.to_lowercase(), search_lower.clone())
                        };

                        // Check for any match in immediate subdirectories
                        if name_compare == search_compare {
                            if is_debug_enabled() {
                                eprintln!("DEBUG: Immediate exact match: {}", path.display());
                            }
                            let dir_match = DirectoryMatch {
                                path: preserve_symlink_in_path(&path, user_current_dir, canonical_current_dir),
                                depth_from_current: 1,
                                match_quality: MatchQuality::ExactDown,
                            };
                            immediate_matches.push(dir_match.clone());
                            all_matches.push(dir_match);
                        } else if name_compare.starts_with(&search_compare) {
                            if is_debug_enabled() {
                                eprintln!("DEBUG: Immediate prefix match: {}", path.display());
                            }
                            let dir_match = DirectoryMatch {
                                path: preserve_symlink_in_path(&path, user_current_dir, canonical_current_dir),
                                depth_from_current: 1,
                                match_quality: MatchQuality::PrefixDown,
                            };
                            immediate_matches.push(dir_match.clone());
                            all_matches.push(dir_match);
                        } else if name_compare.contains(&search_compare) {
                            if is_debug_enabled() {
                                eprintln!("DEBUG: Immediate partial match: {}", path.display());
                            }
                            let dir_match = DirectoryMatch {
                                path: preserve_symlink_in_path(&path, user_current_dir, canonical_current_dir),
                                depth_from_current: 1,
                                match_quality: MatchQuality::PartialDown,
                            };
                            immediate_matches.push(dir_match.clone());
                            all_matches.push(dir_match);
                        }

                        // Add subdirectories to queue for potential deeper search
                        queue.push_back((path.clone(), 1));
                    }
                }
            }
        }
    }

    // If there are exact or prefix matches in immediate subdirectories, return early to avoid deep search
    let has_good_immediate = immediate_matches.iter().any(|m| {
        matches!(
            m.match_quality,
            MatchQuality::ExactDown | MatchQuality::PrefixDown
        )
    });
    if has_good_immediate {
        if is_debug_enabled() {
            eprintln!("DEBUG: Found good immediate matches, skipping deep search");
        }
        return finalize_matches(all_matches);
    }

    if is_debug_enabled() {
        eprintln!("DEBUG: No good immediate matches, continuing with deep search");
    }

    // Otherwise, continue with breadth-first search for deeper levels
    while let Some((current_path, depth)) = queue.pop_front() {
        if depth == 0 || depth > max_depth {
            continue; // Skip depth 0 (already processed) and beyond max depth
        }
        if is_debug_enabled() {
            eprintln!(
                "DEBUG: Searching depth {} in {}",
                depth,
                current_path.display()
            );
        }

        let mut level_matches = Vec::new();
        let mut level_subdirs = Vec::new();

        if let Ok(entries) = fs::read_dir(&current_path) {
            // Collect and sort entries for deterministic order
            let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
            entries.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

            // Process all entries at this level
            for entry in &entries {
                if let Ok(metadata) = entry.metadata() {
                    if metadata.is_dir() {
                        let path = entry.path();
                        if let Some(name) = path.file_name() {
                            let name_str = name.to_string_lossy();

                            // Check if this directory should be ignored
                            if should_ignore_directory(&name_str, ignore_patterns) {
                                if is_debug_enabled() {
                                    eprintln!(
                                        "DEBUG: Ignoring directory at depth {}: {}",
                                        depth + 1,
                                        name_str
                                    );
                                }
                                continue;
                            }

                            let (name_compare, search_compare) = if case_sensitive {
                                (name_str.to_string(), search_term.to_string())
                            } else {
                                (name_str.to_lowercase(), search_lower.clone())
                            };

                            // Check for any match (exact, prefix, or partial)
                            if name_compare == search_compare {
                                if is_debug_enabled() {
                                    eprintln!(
                                        "DEBUG: Deep exact match at depth {}: {}",
                                        depth + 1,
                                        path.display()
                                    );
                                }
                                level_matches.push(DirectoryMatch {
                                    path: preserve_symlink_in_path(&path, user_current_dir, canonical_current_dir),
                                    depth_from_current: (depth + 1) as i32,
                                    match_quality: MatchQuality::ExactDown,
                                });
                            } else if name_compare.starts_with(&search_compare) {
                                if is_debug_enabled() {
                                    eprintln!(
                                        "DEBUG: Deep prefix match at depth {}: {}",
                                        depth + 1,
                                        path.display()
                                    );
                                }
                                level_matches.push(DirectoryMatch {
                                    path: preserve_symlink_in_path(&path, user_current_dir, canonical_current_dir),
                                    depth_from_current: (depth + 1) as i32,
                                    match_quality: MatchQuality::PrefixDown,
                                });
                            } else if name_compare.contains(&search_compare) {
                                if is_debug_enabled() {
                                    eprintln!(
                                        "DEBUG: Deep partial match at depth {}: {}",
                                        depth + 1,
                                        path.display()
                                    );
                                }
                                level_matches.push(DirectoryMatch {
                                    path: preserve_symlink_in_path(&path, user_current_dir, canonical_current_dir),
                                    depth_from_current: (depth + 1) as i32,
                                    match_quality: MatchQuality::PartialDown,
                                });
                            }

                            // Collect subdirectories for next level
                            if depth < max_depth {
                                level_subdirs.push((path.clone(), depth + 1));
                            }
                        }
                    }
                }
            }
        }

        // Add matches from this level
        all_matches.extend(level_matches);

        // Add subdirectories to queue for next level search
        for (subdir, next_depth) in level_subdirs {
            queue.push_back((subdir, next_depth));
        }
    }

    if is_debug_enabled() {
        eprintln!(
            "DEBUG: search_down_breadth_first_all completed with {} total matches",
            all_matches.len()
        );
    }

    finalize_matches(all_matches)
}

fn finalize_matches(mut matches: Vec<DirectoryMatch>) -> Vec<DirectoryMatch> {
    // Remove duplicates based on path
    matches.sort_by(|a, b| a.path.cmp(&b.path));
    matches.dedup_by(|a, b| a.path == b.path);

    // Sort by priority with clear prioritization
    matches.sort_by(|a, b| {
        // Define priority categories
        let get_priority = |m: &DirectoryMatch| -> u32 {
            match (m.depth_from_current, &m.match_quality) {
                // Immediate subdirectory exact matches - highest priority
                (1, MatchQuality::ExactDown) => 0,
                // Immediate subdirectory prefix matches - very high priority
                (1, MatchQuality::PrefixDown) => 1,
                // Immediate subdirectory partial matches - high priority
                (1, MatchQuality::PartialDown) => 2,
                // Up tree exact matches - medium-high priority
                (_, MatchQuality::ExactUp) => 3,
                // Up tree partial matches - medium priority
                (_, MatchQuality::PartialUp) => 4,
                // Deeper exact matches - lower priority
                (_, MatchQuality::ExactDown) => 5,
                // Deeper prefix matches - lower priority
                (_, MatchQuality::PrefixDown) => 6,
                // Deeper partial matches - lowest priority
                (_, MatchQuality::PartialDown) => 7,
            }
        };

        let a_priority = get_priority(a);
        let b_priority = get_priority(b);

        // First sort by priority
        let priority_cmp = a_priority.cmp(&b_priority);
        if priority_cmp != std::cmp::Ordering::Equal {
            return priority_cmp;
        }

        // Within same priority, sort by depth (shallower first for down matches, closer first for up matches)
        match a.match_quality {
            MatchQuality::ExactUp | MatchQuality::PartialUp => {
                // For up matches, closer to current (higher depth) comes first
                b.depth_from_current.cmp(&a.depth_from_current)
            }
            _ => {
                // For down matches, shallower (lower depth) comes first
                a.depth_from_current.cmp(&b.depth_from_current)
            }
        }
    });

    matches
}

fn search_path_pattern_fast(
    current_dir: &Path,
    search_term: &str,
    matches: &mut Vec<DirectoryMatch>,
    context: &mut SearchContext,
    case_sensitive: bool,
    user_current_dir: &Path,
    canonical_current_dir: &Path,
) {
    if is_debug_enabled() {
        eprintln!(
            "DEBUG: search_path_pattern_fast: current_dir={}, search_term='{}', case_sensitive={}",
            current_dir.display(),
            search_term,
            case_sensitive
        );
    }

    let parts: Vec<&str> = search_term.split('/').collect();
    if parts.is_empty() || !context.should_continue() {
        if is_debug_enabled() {
            eprintln!(
                "DEBUG: search_path_pattern_fast: early exit - parts empty or context expired"
            );
        }
        return;
    }

    let first_part = parts[0];
    let remaining_parts = &parts[1..];

    if is_debug_enabled() {
        eprintln!(
            "DEBUG: search_path_pattern_fast: split into first_part='{}', remaining_parts={:?}",
            first_part, remaining_parts
        );
    }

    // Search for the first part in current directory and subdirectories
    if is_debug_enabled() {
        eprintln!(
            "DEBUG: search_path_pattern_fast: starting recursive search down from current dir"
        );
    }
    search_pattern_recursive_fast(
        current_dir,
        first_part,
        remaining_parts,
        matches,
        context,
        0,
        4,
        case_sensitive,
        user_current_dir,
        canonical_current_dir,
    );

    // Also search up the tree for the first part (but limit this to avoid slowdown)
    if is_debug_enabled() {
        eprintln!("DEBUG: search_path_pattern_fast: starting search up the tree");
    }
    let mut current = current_dir;
    let mut depth = -1;
    let mut up_count = 0;

    while let Some(parent) = current.parent() {
        if !context.should_continue() || up_count >= 10 {
            if is_debug_enabled() {
                eprintln!("DEBUG: search_path_pattern_fast: stopping up search - context expired or max up count reached");
            }
            break;
        }

        if let Some(name) = parent.file_name() {
            let name_str = name.to_string_lossy();
            if is_debug_enabled() {
                eprintln!(
                    "DEBUG: search_path_pattern_fast: checking parent '{}' at depth {}",
                    name_str, depth
                );
            }

            let matches_pattern = if case_sensitive {
                name_str.contains(first_part)
            } else {
                name_str.to_lowercase().contains(&first_part.to_lowercase())
            };

            if matches_pattern {
                if is_debug_enabled() {
                    eprintln!(
                        "DEBUG: search_path_pattern_fast: parent '{}' contains pattern '{}'",
                        name_str, first_part
                    );
                }

                if remaining_parts.is_empty() {
                    let match_quality = if name_str.to_lowercase() == first_part.to_lowercase() {
                        MatchQuality::ExactUp
                    } else {
                        MatchQuality::PartialUp
                    };

                    if is_debug_enabled() {
                        eprintln!(
                            "DEBUG: search_path_pattern_fast: adding up match {:?} for {}",
                            match_quality,
                            parent.display()
                        );
                    }

                    matches.push(DirectoryMatch {
                        path: preserve_symlink_in_path(parent, user_current_dir, canonical_current_dir),
                        depth_from_current: depth,
                        match_quality,
                    });
                    context.add_match();
                } else {
                    if is_debug_enabled() {
                        eprintln!("DEBUG: search_path_pattern_fast: recursing from parent for remaining patterns");
                    }
                    search_pattern_recursive_fast(
                        parent,
                        &remaining_parts[0],
                        &remaining_parts[1..],
                        matches,
                        context,
                        depth,
                        3,
                        case_sensitive,
                        user_current_dir,
                        canonical_current_dir,
                    );
                }
            }
        }
        current = parent;
        depth -= 1;
        up_count += 1;
    }

    if is_debug_enabled() {
        eprintln!(
            "DEBUG: search_path_pattern_fast: completed with {} total matches",
            matches.len()
        );
    }
}

fn search_pattern_recursive_fast(
    current_dir: &Path,
    pattern: &str,
    remaining_patterns: &[&str],
    matches: &mut Vec<DirectoryMatch>,
    context: &mut SearchContext,
    base_depth: i32,
    max_depth: usize,
    case_sensitive: bool,
    user_current_dir: &Path,
    canonical_current_dir: &Path,
) {
    if is_debug_enabled() {
        eprintln!("DEBUG: search_pattern_recursive_fast: dir={}, pattern='{}', remaining={:?}, base_depth={}, max_depth={}, case_sensitive={}",
                 current_dir.display(), pattern, remaining_patterns, base_depth, max_depth, case_sensitive);
    }

    if max_depth == 0 || !context.should_continue() {
        if is_debug_enabled() {
            eprintln!(
                "DEBUG: search_pattern_recursive_fast: early exit - max_depth=0 or context expired"
            );
        }
        return;
    }

    if let Ok(entries) = fs::read_dir(current_dir) {
        let mut entry_count = 0;
        let mut match_count = 0;

        for entry in entries.flatten() {
            entry_count += 1;

            if !context.should_continue() {
                if is_debug_enabled() {
                    eprintln!(
                        "DEBUG: search_pattern_recursive_fast: breaking due to context timeout"
                    );
                }
                break;
            }

            if let Ok(metadata) = entry.metadata() {
                if metadata.is_dir() {
                    let path = entry.path();
                    if let Some(name) = path.file_name() {
                        let name_str = name.to_string_lossy();
                        let matches_pattern = if case_sensitive {
                            name_str.contains(pattern)
                        } else {
                            name_str.to_lowercase().contains(&pattern.to_lowercase())
                        };

                        if matches_pattern {
                            match_count += 1;

                            if is_debug_enabled() {
                                eprintln!("DEBUG: search_pattern_recursive_fast: found matching dir '{}' for pattern '{}'", name_str, pattern);
                            }

                            if remaining_patterns.is_empty() {
                                let is_exact = if case_sensitive {
                                    name_str == pattern
                                } else {
                                    name_str.to_lowercase() == pattern.to_lowercase()
                                };

                                let match_quality = if is_exact {
                                    if base_depth < 0 {
                                        MatchQuality::ExactUp
                                    } else {
                                        MatchQuality::ExactDown
                                    }
                                } else {
                                    if base_depth < 0 {
                                        MatchQuality::PartialUp
                                    } else {
                                        MatchQuality::PartialDown
                                    }
                                };

                                if is_debug_enabled() {
                                    eprintln!("DEBUG: search_pattern_recursive_fast: adding final match {:?} for {}", match_quality, path.display());
                                }

                                matches.push(DirectoryMatch {
                                    path: preserve_symlink_in_path(&path, user_current_dir, canonical_current_dir),
                                    depth_from_current: base_depth + 1,
                                    match_quality,
                                });
                                context.add_match();
                            } else {
                                if is_debug_enabled() {
                                    eprintln!("DEBUG: search_pattern_recursive_fast: recursing deeper for remaining patterns");
                                }
                                search_pattern_recursive_fast(
                                    &path,
                                    remaining_patterns[0],
                                    &remaining_patterns[1..],
                                    matches,
                                    context,
                                    base_depth + 1,
                                    max_depth - 1,
                                    case_sensitive,
                                    user_current_dir,
                                    canonical_current_dir,
                                );
                            }
                        }

                        // Also recurse into subdirectories to find pattern deeper
                        if context.should_continue() {
                            search_pattern_recursive_fast(
                                &path,
                                pattern,
                                remaining_patterns,
                                matches,
                                context,
                                base_depth + 1,
                                max_depth - 1,
                                case_sensitive,
                                user_current_dir,
                                canonical_current_dir,
                            );
                        }
                    }
                }
            }
        }

        if is_debug_enabled() {
            eprintln!("DEBUG: search_pattern_recursive_fast: processed {} entries, found {} pattern matches in {}",
                     entry_count, match_count, current_dir.display());
        }
    } else if is_debug_enabled() {
        eprintln!(
            "DEBUG: search_pattern_recursive_fast: failed to read directory {}",
            current_dir.display()
        );
    }
}

fn search_absolute_pattern(
    parent_dir: &Path,
    pattern: &str,
    matches: &mut Vec<DirectoryMatch>,
    case_sensitive: bool,
) {
    use std::collections::VecDeque;

    let mut queue = VecDeque::new();
    let mut immediate_matches: Vec<DirectoryMatch> = Vec::new();
    queue.push_back((parent_dir.to_path_buf(), 0));
    let search_lower = if case_sensitive {
        pattern.to_string()
    } else {
        pattern.to_lowercase()
    };
    let max_depth = 8;

    // First, search immediate subdirectories (depth 1) to check for early stopping
    if let Ok(entries) = fs::read_dir(parent_dir) {
        let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
        entries.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

        for entry in &entries {
            if let Ok(metadata) = entry.metadata() {
                if metadata.is_dir() {
                    let path = entry.path();
                    if let Some(name) = path.file_name() {
                        let name_str = name.to_string_lossy();
                        let (name_compare, search_compare) = if case_sensitive {
                            (name_str.to_string(), pattern.to_string())
                        } else {
                            (name_str.to_lowercase(), search_lower.clone())
                        };

                        // Check for immediate matches
                        if name_compare == search_compare {
                            let dir_match = DirectoryMatch {
                                path: path.clone(),
                                depth_from_current: 1,
                                match_quality: MatchQuality::ExactDown,
                            };
                            immediate_matches.push(dir_match.clone());
                            matches.push(dir_match);
                        } else if name_compare.starts_with(&search_compare) {
                            let dir_match = DirectoryMatch {
                                path: path.clone(),
                                depth_from_current: 1,
                                match_quality: MatchQuality::PrefixDown,
                            };
                            immediate_matches.push(dir_match.clone());
                            matches.push(dir_match);
                        } else if name_compare.contains(&search_compare) {
                            let dir_match = DirectoryMatch {
                                path: path.clone(),
                                depth_from_current: 1,
                                match_quality: MatchQuality::PartialDown,
                            };
                            immediate_matches.push(dir_match.clone());
                            matches.push(dir_match);
                        }

                        // Add subdirectories to queue for potential deeper search
                        queue.push_back((path.clone(), 1));
                    }
                }
            }
        }
    }

    // If there are any matches in immediate subdirectories, return early to avoid deep search
    // This prioritizes local matches over distant ones (same logic as relative paths)
    if !immediate_matches.is_empty() {
        return;
    }

    // Otherwise, continue with breadth-first search for deeper levels
    while let Some((current_dir, depth)) = queue.pop_front() {
        if depth == 0 || depth > max_depth {
            continue; // Skip depth 0 (already processed) and beyond max depth
        }

        if let Ok(entries) = fs::read_dir(&current_dir) {
            let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
            entries.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

            for entry in &entries {
                if let Ok(metadata) = entry.metadata() {
                    if metadata.is_dir() {
                        let path = entry.path();
                        if let Some(name) = path.file_name() {
                            let name_str = name.to_string_lossy();
                            let (name_compare, search_compare) = if case_sensitive {
                                (name_str.to_string(), pattern.to_string())
                            } else {
                                (name_str.to_lowercase(), search_lower.clone())
                            };

                            // Check for pattern match at deeper levels
                            if name_compare == search_compare {
                                matches.push(DirectoryMatch {
                                    path: path.clone(),
                                    depth_from_current: depth as i32,
                                    match_quality: MatchQuality::ExactDown,
                                });
                            } else if name_compare.starts_with(&search_compare) {
                                matches.push(DirectoryMatch {
                                    path: path.clone(),
                                    depth_from_current: depth as i32,
                                    match_quality: MatchQuality::PrefixDown,
                                });
                            } else if name_compare.contains(&search_compare) {
                                matches.push(DirectoryMatch {
                                    path: path.clone(),
                                    depth_from_current: depth as i32,
                                    match_quality: MatchQuality::PartialDown,
                                });
                            }

                            // Add subdirectories to queue for next level search
                            if depth < max_depth {
                                queue.push_back((path, depth + 1));
                            }
                        }
                    }
                }
            }
        }
    }
}

fn find_search_root_and_pattern(search_term: &str) -> (Option<PathBuf>, String) {
    let path = Path::new(search_term);
    let mut current = path;

    // Walk up the path to find the longest existing prefix
    while let Some(parent) = current.parent() {
        if parent.exists() && parent.is_dir() {
            // Found existing parent directory
            // The search pattern is the first component after this parent
            let remaining = path.strip_prefix(parent).unwrap();
            let mut components = remaining.components();
            if let Some(first_component) = components.next() {
                let pattern = first_component.as_os_str().to_string_lossy().to_string();
                return (Some(parent.to_path_buf()), pattern);
            }
        }
        current = parent;
    }

    // If we get here, even root doesn't exist (shouldn't happen on Unix)
    // Fall back to searching from root with the first component as pattern
    let first_component = Path::new(search_term)
        .components()
        .nth(1) // Skip the root component "/"
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .unwrap_or_else(|| search_term.trim_start_matches('/').to_string());
    (Some(PathBuf::from("/")), first_component)
}

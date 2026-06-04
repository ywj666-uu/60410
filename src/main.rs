use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use clap::Parser;
use comfy_table::{Cell, Table};
use petgraph::graph::{DiGraph, NodeIndex};
use regex::Regex;
use walkdir::WalkDir;

#[derive(Parser)]
#[command(name = "pycycle-detector")]
#[command(about = "检测 Python 项目中的循环导入依赖")]
struct Cli {
    /// 要扫描的目录路径
    #[arg(default_value = ".")]
    path: PathBuf,

    /// 忽略的目录（可多次指定，支持绝对或相对路径）
    #[arg(short, long, value_delimiter = ',')]
    ignore: Vec<String>,

    /// 输出 DOT 格式的文件路径
    #[arg(short, long, default_value = "dependencies.dot")]
    output: PathBuf,

    /// 仅显示包含循环的子图
    #[arg(long, default_value_t = false)]
    cycles_only: bool,
}

const DEFAULT_IGNORE_NAMES: &[&str] = &[
    "venv",
    ".venv",
    "env",
    ".env",
    "ENV",
    "virtualenv",
    "__pycache__",
    ".git",
    ".svn",
    "node_modules",
    ".tox",
    ".nox",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    "site-packages",
    "dist-packages",
    ".eggs",
    "build",
    "dist",
    ".hg",
];

fn main() {
    let cli = Cli::parse();
    let root = cli.path.canonicalize().unwrap_or_else(|_| cli.path.clone());

    let ignore_abs_paths: Vec<PathBuf> = cli
        .ignore
        .iter()
        .map(|p| {
            let pb = PathBuf::from(p);
            if pb.is_absolute() {
                pb
            } else {
                root.join(&pb)
            }
        })
        .collect();

    let default_names: HashSet<&str> = DEFAULT_IGNORE_NAMES.iter().copied().collect();

    let python_files = collect_python_files(&root, &default_names, &ignore_abs_paths);
    if python_files.is_empty() {
        println!("未找到 Python 文件。");
        return;
    }

    println!("扫描到 {} 个 Python 文件", python_files.len());

    let module_map = build_module_map(&python_files, &root);
    let dependencies = extract_all_dependencies(&python_files, &module_map, &root);

    let (graph, node_indices, index_to_name) = build_graph(&dependencies, &module_map);

    let cycles = find_all_cycles(&graph, &node_indices, &index_to_name, &dependencies);

    print_cycles(&cycles);
    print_degree_table(&graph, &index_to_name);

    let cycle_node_set = cycles_to_node_set(&cycles, &node_indices);
    let dot = generate_dot(&graph, &index_to_name, &cycle_node_set, cli.cycles_only);
    fs::write(&cli.output, &dot).expect("无法写入 DOT 文件");
    println!("\nDOT 图已写入: {}", cli.output.display());

    if cycles.is_empty() {
        println!("\n✓ 未检测到循环导入。");
    } else {
        println!("\n✗ 检测到 {} 个循环依赖链。", cycles.len());
        std::process::exit(1);
    }
}

fn collect_python_files(
    root: &Path,
    default_names: &HashSet<&str>,
    ignore_abs_paths: &[PathBuf],
) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in WalkDir::new(root).into_iter().filter_entry(|e| {
        if e.file_type().is_dir() {
            let dir_name = e.file_name().to_string_lossy();
            if default_names.contains(dir_name.as_ref()) {
                return false;
            }
            let abs = e.path().to_path_buf();
            let canon = abs.canonicalize().unwrap_or(abs);
            for ignored in ignore_abs_paths {
                let ignored_canon = ignored.canonicalize().unwrap_or_else(|_| ignored.clone());
                if canon == ignored_canon || canon.starts_with(&ignored_canon) {
                    return false;
                }
            }
            true
        } else {
            true
        }
    }) {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.file_type().is_file() {
            let path = entry.path();
            if path.extension().map_or(false, |ext| ext == "py") {
                files.push(path.to_path_buf());
            }
        }
    }
    files
}

fn path_to_module(path: &Path, root: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let mut parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_str().unwrap_or("").to_string())
        .collect();

    if let Some(last) = parts.last_mut() {
        if let Some(name) = last.strip_suffix(".py") {
            *last = name.to_string();
        }
    }

    let module = parts.join(".");

    if module.ends_with(".__init__") {
        module.trim_end_matches(".__init__").to_string()
    } else {
        module
    }
}

fn is_package_init(path: &Path) -> bool {
    path.file_name()
        .map_or(false, |n| n.to_string_lossy() == "__init__.py")
}

fn build_module_map(files: &[PathBuf], root: &Path) -> HashMap<String, PathBuf> {
    let mut map = HashMap::new();
    for file in files {
        let module = path_to_module(file, root);
        map.insert(module, file.clone());
    }
    map
}

#[derive(Debug, Clone)]
struct RawImport {
    base: String,
    names: Vec<String>,
    is_relative: bool,
    dot_count: usize,
}

fn extract_imports(file: &Path) -> Vec<RawImport> {
    let content = match fs::read_to_string(file) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let re_import = Regex::new(r"(?m)^\s*import\s+([\w.]+(?:\s*,\s*[\w.]+)*)").unwrap();
    let re_from =
        Regex::new(r"(?m)^\s*from\s+(\.+[\w.]*|[\w.]+)\s+import\s+([^#\n]+)").unwrap();

    let mut imports = Vec::new();

    for cap in re_import.captures_iter(&content) {
        let modules_str = &cap[1];
        for m in modules_str.split(',') {
            let m = m.trim().split_whitespace().next().unwrap_or("").trim();
            if !m.is_empty() {
                imports.push(RawImport {
                    base: m.to_string(),
                    names: vec![],
                    is_relative: false,
                    dot_count: 0,
                });
            }
        }
    }

    for cap in re_from.captures_iter(&content) {
        let source = cap[1].trim();
        let names_str = &cap[2];

        let dot_count = source.chars().take_while(|c| *c == '.').count();
        let is_relative = dot_count > 0;
        let base_module = &source[dot_count..];

        let mut names = Vec::new();
        for name in names_str.split(',') {
            let name = name.trim();
            let name = name.split_whitespace().next().unwrap_or("").trim();
            if !name.is_empty() && name != "*" {
                names.push(name.to_string());
            }
        }

        imports.push(RawImport {
            base: base_module.to_string(),
            names,
            is_relative,
            dot_count,
        });
    }

    imports
}

fn resolve_import(
    importing_module: &str,
    importing_file: &Path,
    raw: &RawImport,
    module_map: &HashMap<String, PathBuf>,
) -> Vec<String> {
    let known_modules: HashSet<&str> = module_map.keys().map(|s| s.as_str()).collect();
    let mut resolved = Vec::new();

    if raw.is_relative {
        let package_base = compute_relative_base(importing_module, importing_file, raw.dot_count);
        if let Some(base) = package_base {
            let full_base = if raw.base.is_empty() {
                base.clone()
            } else {
                format!("{}.{}", base, raw.base)
            };

            try_resolve_module(&full_base, &raw.names, &known_modules, &mut resolved);
        }
    } else {
        try_resolve_module(&raw.base, &raw.names, &known_modules, &mut resolved);
    }

    resolved
}

fn compute_relative_base(
    importing_module: &str,
    importing_file: &Path,
    dot_count: usize,
) -> Option<String> {
    let parts: Vec<&str> = importing_module.split('.').collect();

    let is_init = is_package_init(importing_file);
    // `from . import X` in __init__.py means current package = the module itself
    // `from . import X` in a regular module means parent package
    let effective_levels = if is_init { dot_count - 1 } else { dot_count };

    if effective_levels >= parts.len() {
        return None;
    }

    let keep = parts.len() - effective_levels;
    let base = parts[..keep].join(".");
    if base.is_empty() {
        None
    } else {
        Some(base)
    }
}

fn try_resolve_module(
    full_base: &str,
    names: &[String],
    known_modules: &HashSet<&str>,
    resolved: &mut Vec<String>,
) {
    if names.is_empty() {
        if known_modules.contains(full_base) {
            resolved.push(full_base.to_string());
        } else {
            find_longest_prefix(full_base, known_modules, resolved);
        }
        return;
    }

    for name in names {
        let full_path = format!("{}.{}", full_base, name);
        if known_modules.contains(full_path.as_str()) {
            resolved.push(full_path);
        } else if known_modules.contains(full_base) {
            resolved.push(full_base.to_string());
        } else {
            find_longest_prefix(&full_path, known_modules, resolved);
        }
    }
}

fn find_longest_prefix(target: &str, known_modules: &HashSet<&str>, resolved: &mut Vec<String>) {
    let mut best: Option<&str> = None;
    for &m in known_modules {
        if target.starts_with(m)
            && (target.len() == m.len() || target.as_bytes()[m.len()] == b'.')
        {
            if best.map_or(true, |b| m.len() > b.len()) {
                best = Some(m);
            }
        }
    }
    if let Some(b) = best {
        resolved.push(b.to_string());
    }
}

fn extract_all_dependencies(
    files: &[PathBuf],
    module_map: &HashMap<String, PathBuf>,
    root: &Path,
) -> HashMap<String, Vec<String>> {
    let mut deps: HashMap<String, Vec<String>> = HashMap::new();

    for file in files {
        let module_name = path_to_module(file, root);
        let raw_imports = extract_imports(file);
        let mut module_deps: HashSet<String> = HashSet::new();

        for raw in &raw_imports {
            let resolved = resolve_import(&module_name, file, raw, module_map);
            for r in resolved {
                if r != module_name {
                    module_deps.insert(r);
                }
            }
        }

        deps.insert(module_name, module_deps.into_iter().collect());
    }

    deps
}

fn build_graph(
    dependencies: &HashMap<String, Vec<String>>,
    module_map: &HashMap<String, PathBuf>,
) -> (
    DiGraph<String, ()>,
    HashMap<String, NodeIndex>,
    HashMap<NodeIndex, String>,
) {
    let mut graph = DiGraph::new();
    let mut node_indices: HashMap<String, NodeIndex> = HashMap::new();
    let mut index_to_name: HashMap<NodeIndex, String> = HashMap::new();

    for module in module_map.keys() {
        let idx = graph.add_node(module.clone());
        node_indices.insert(module.clone(), idx);
        index_to_name.insert(idx, module.clone());
    }

    for (module, imports) in dependencies {
        if let Some(&from_idx) = node_indices.get(module) {
            for imp in imports {
                if let Some(&to_idx) = node_indices.get(imp) {
                    if from_idx != to_idx {
                        graph.add_edge(from_idx, to_idx, ());
                    }
                }
            }
        }
    }

    (graph, node_indices, index_to_name)
}

/// DFS-based cycle detection that records the full path of each cycle
fn find_all_cycles(
    graph: &DiGraph<String, ()>,
    node_indices: &HashMap<String, NodeIndex>,
    index_to_name: &HashMap<NodeIndex, String>,
    _dependencies: &HashMap<String, Vec<String>>,
) -> Vec<Vec<String>> {
    let mut cycles: Vec<Vec<String>> = Vec::new();
    let mut visited: HashSet<NodeIndex> = HashSet::new();
    let mut on_stack: HashSet<NodeIndex> = HashSet::new();
    let mut path: Vec<NodeIndex> = Vec::new();
    let mut seen_cycles: HashSet<Vec<String>> = HashSet::new();

    for &start in node_indices.values() {
        if !visited.contains(&start) {
            dfs_find_cycles(
                graph,
                start,
                &mut visited,
                &mut on_stack,
                &mut path,
                index_to_name,
                &mut cycles,
                &mut seen_cycles,
            );
        }
    }

    cycles
}

fn dfs_find_cycles(
    graph: &DiGraph<String, ()>,
    node: NodeIndex,
    visited: &mut HashSet<NodeIndex>,
    on_stack: &mut HashSet<NodeIndex>,
    path: &mut Vec<NodeIndex>,
    index_to_name: &HashMap<NodeIndex, String>,
    cycles: &mut Vec<Vec<String>>,
    seen_cycles: &mut HashSet<Vec<String>>,
) {
    visited.insert(node);
    on_stack.insert(node);
    path.push(node);

    for neighbor in graph.neighbors(node) {
        if !visited.contains(&neighbor) {
            dfs_find_cycles(
                graph, neighbor, visited, on_stack, path, index_to_name, cycles, seen_cycles,
            );
        } else if on_stack.contains(&neighbor) {
            let cycle_start = path.iter().position(|&n| n == neighbor).unwrap();
            let cycle_path: Vec<String> = path[cycle_start..]
                .iter()
                .map(|idx| index_to_name.get(idx).cloned().unwrap_or_default())
                .collect();

            let canonical = canonicalize_cycle(&cycle_path);
            if !seen_cycles.contains(&canonical) {
                seen_cycles.insert(canonical);
                cycles.push(cycle_path);
            }
        }
    }

    path.pop();
    on_stack.remove(&node);
}

fn canonicalize_cycle(cycle: &[String]) -> Vec<String> {
    if cycle.is_empty() {
        return vec![];
    }
    let min_pos = cycle
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| a.cmp(b))
        .map(|(i, _)| i)
        .unwrap_or(0);

    let mut canonical: Vec<String> = cycle[min_pos..].to_vec();
    canonical.extend_from_slice(&cycle[..min_pos]);
    canonical
}

fn cycles_to_node_set(
    cycles: &[Vec<String>],
    node_indices: &HashMap<String, NodeIndex>,
) -> HashSet<NodeIndex> {
    let mut set = HashSet::new();
    for cycle in cycles {
        for name in cycle {
            if let Some(&idx) = node_indices.get(name) {
                set.insert(idx);
            }
        }
    }
    set
}

fn print_cycles(cycles: &[Vec<String>]) {
    if cycles.is_empty() {
        return;
    }

    println!("\n=== 检测到的循环依赖 ===\n");
    for (i, cycle) in cycles.iter().enumerate() {
        println!("循环 #{} (长度 {})", i + 1, cycle.len());
        print!("  ");
        for (j, name) in cycle.iter().enumerate() {
            if j > 0 {
                print!(" -> ");
            }
            print!("{}", name);
        }
        println!(" -> {}", cycle.first().unwrap_or(&"?".to_string()));
        println!();
    }
}

fn print_degree_table(graph: &DiGraph<String, ()>, index_to_name: &HashMap<NodeIndex, String>) {
    let mut table = Table::new();
    table.set_header(vec![
        Cell::new("模块"),
        Cell::new("入度 (被导入次数)"),
        Cell::new("出度 (导入他人次数)"),
    ]);

    let mut rows: Vec<(&str, usize, usize)> = graph
        .node_indices()
        .map(|idx| {
            let name = index_to_name.get(&idx).map(|s| s.as_str()).unwrap_or("?");
            let in_deg = graph
                .neighbors_directed(idx, petgraph::Direction::Incoming)
                .count();
            let out_deg = graph
                .neighbors_directed(idx, petgraph::Direction::Outgoing)
                .count();
            (name, in_deg, out_deg)
        })
        .filter(|(_, ind, outd)| *ind > 0 || *outd > 0)
        .collect();

    rows.sort_by(|a, b| (b.1 + b.2).cmp(&(a.1 + a.2)));

    for (name, in_deg, out_deg) in &rows {
        table.add_row(vec![
            Cell::new(name),
            Cell::new(in_deg),
            Cell::new(out_deg),
        ]);
    }

    println!("\n=== 模块依赖度统计 ===\n");
    println!("{table}");
}

fn generate_dot(
    graph: &DiGraph<String, ()>,
    index_to_name: &HashMap<NodeIndex, String>,
    cycle_nodes: &HashSet<NodeIndex>,
    cycles_only: bool,
) -> String {
    let mut dot = String::new();
    dot.push_str("digraph dependencies {\n");
    dot.push_str("    rankdir=LR;\n");
    dot.push_str("    node [shape=box, style=filled, fillcolor=lightyellow];\n");
    dot.push_str("    // 红色节点表示参与循环依赖的模块\n\n");

    for idx in graph.node_indices() {
        let name = index_to_name.get(&idx).map(|s| s.as_str()).unwrap_or("?");
        let in_cycle = cycle_nodes.contains(&idx);

        if cycles_only && !in_cycle {
            continue;
        }

        if in_cycle {
            dot.push_str(&format!(
                "    \"{}\" [fillcolor=salmon, color=red, penwidth=2];\n",
                name
            ));
        } else {
            dot.push_str(&format!("    \"{}\";\n", name));
        }
    }

    dot.push('\n');

    for edge in graph.edge_indices() {
        let (src, dst) = graph.edge_endpoints(edge).unwrap();
        let src_name = index_to_name.get(&src).map(|s| s.as_str()).unwrap_or("?");
        let dst_name = index_to_name.get(&dst).map(|s| s.as_str()).unwrap_or("?");

        let both_in_cycle = cycle_nodes.contains(&src) && cycle_nodes.contains(&dst);

        if cycles_only && !both_in_cycle {
            continue;
        }

        if both_in_cycle {
            dot.push_str(&format!(
                "    \"{}\" -> \"{}\" [color=red, penwidth=2];\n",
                src_name, dst_name
            ));
        } else {
            dot.push_str(&format!("    \"{}\" -> \"{}\";\n", src_name, dst_name));
        }
    }

    dot.push_str("}\n");
    dot
}

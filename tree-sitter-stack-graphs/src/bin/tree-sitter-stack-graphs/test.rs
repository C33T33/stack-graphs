// -*- coding: utf-8 -*-
// ------------------------------------------------------------------------------------------------
// Copyright © 2021, stack-graphs authors.
// Licensed under either of Apache License, Version 2.0, or MIT license, at your option.
// Please see the LICENSE-APACHE or LICENSE-MIT files in this distribution for license details.
// ------------------------------------------------------------------------------------------------

use anyhow::anyhow;
use anyhow::Context as _;
use clap::ArgEnum;
use clap::ValueHint;
use colored::Colorize as _;
use stack_graphs::arena::Handle;
use stack_graphs::graph::File;
use stack_graphs::graph::StackGraph;
use stack_graphs::json::Filter;
use stack_graphs::paths::Paths;
use std::path::Path;
use std::path::PathBuf;
use tree_sitter_graph::Variables;
use tree_sitter_stack_graphs::loader::Loader;
use tree_sitter_stack_graphs::test::Test;
use tree_sitter_stack_graphs::test::TestResult;
use tree_sitter_stack_graphs::LoadError;
use tree_sitter_stack_graphs::NoCancellation;
use tree_sitter_stack_graphs::StackGraphLanguage;
use walkdir::WalkDir;

use crate::loader::LoaderArgs;
use crate::util::map_parse_errors;
use crate::util::path_exists;
use crate::util::PathSpec;

/// Flag to control output
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ArgEnum)]
pub enum OutputMode {
    Always,
    OnFailure,
}

impl OutputMode {
    fn test(&self, failure: bool) -> bool {
        match self {
            Self::Always => true,
            Self::OnFailure => failure,
        }
    }
}

/// Run tests
#[derive(clap::Parser)]
#[clap(after_help = r#"PATH SPECIFICATIONS:
    Output filenames can be specified using placeholders based on the input file.
    The following placeholders are supported:
         %r   the root path, which is the directory argument which contains the file,
              or the directory of the file argument
         %d   the path directories relative to the root
         %n   the name of the file
         %e   the file extension (including the preceding dot)
         %%   a literal percentage sign

    Empty directory placeholders (%r and %d) are replaced by "." so that the shape
    of the path is not accidently changed. For example, "test -V %d/%n.html mytest.py"
    results in "./mytest.html" instead of the unintented "/mytest.html".

    Note that on Windows the path specification must be valid Unicode, but all valid
    paths (including ones that are not valid Unicode) are accepted as arguments, and
    placeholders are correctly subtituted for all paths.
"#)]
pub struct Command {
    #[clap(flatten)]
    loader: LoaderArgs,

    /// Test file or directory paths.
    #[clap(value_name = "TEST_PATH", required = true, value_hint = ValueHint::AnyPath, parse(from_os_str), validator_os = path_exists)]
    tests: Vec<PathBuf>,

    /// Hide passing tests.
    #[clap(long)]
    hide_passing: bool,

    /// Hide failure error details.
    #[clap(long)]
    hide_failure_errors: bool,

    /// Show ignored files in output.
    #[clap(long)]
    show_ignored: bool,

    /// Save graph for tests matching output mode.
    /// Takes an optional path specification argument for the output file.
    /// [default: %n.graph.json]
    #[clap(
        long,
        short = 'G',
        value_name = "PATH_SPEC",
        min_values = 0,
        max_values = 1,
        require_equals = true,
        default_missing_value = "%n.graph.json"
    )]
    save_graph: Option<PathSpec>,

    /// Save paths for tests matching output mode.
    /// Takes an optional path specification argument for the output file.
    /// [default: %n.paths.json]
    #[clap(
        long,
        short = 'P',
        value_name = "PATH_SPEC",
        min_values = 0,
        max_values = 1,
        require_equals = true,
        default_missing_value = "%n.paths.json"
    )]
    save_paths: Option<PathSpec>,

    /// Save visualization for tests matching output mode.
    /// Takes an optional path specification argument for the output file.
    /// [default: %n.html]
    #[clap(
        long,
        short = 'V',
        value_name = "PATH_SPEC",
        min_values = 0,
        max_values = 1,
        require_equals = true,
        default_missing_value = "%n.html"
    )]
    save_visualization: Option<PathSpec>,

    /// Controls when graphs, paths, or visualization are saved.
    #[clap(long, arg_enum, default_value_t = OutputMode::OnFailure)]
    output_mode: OutputMode,
}

impl Command {
    pub fn run(&self) -> anyhow::Result<()> {
        let mut loader = self.loader.new_loader()?;
        let mut total_failure_count = 0;
        for test_path in &self.tests {
            if test_path.is_dir() {
                let test_root = test_path;
                for test_entry in WalkDir::new(test_path)
                    .follow_links(true)
                    .into_iter()
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_type().is_file())
                {
                    let test_path = test_entry.path();
                    total_failure_count +=
                        self.run_test_with_context(test_root, test_path, &mut loader)?;
                }
            } else {
                let test_root = test_path.parent().unwrap();
                total_failure_count +=
                    self.run_test_with_context(test_root, test_path, &mut loader)?;
            }
        }

        if total_failure_count > 0 {
            return Err(anyhow!(
                "{} assertion{} failed",
                total_failure_count,
                if total_failure_count == 1 { "" } else { "s" }
            ));
        }

        Ok(())
    }

    /// Run test file and add error context to any failures that are returned.
    fn run_test_with_context(
        &self,
        test_root: &Path,
        test_path: &Path,
        loader: &mut Loader,
    ) -> anyhow::Result<usize> {
        self.run_test(test_root, test_path, loader)
            .with_context(|| format!("Error running test {}", test_path.display()))
    }

    /// Run test file.
    fn run_test(
        &self,
        test_root: &Path,
        test_path: &Path,
        loader: &mut Loader,
    ) -> anyhow::Result<usize> {
        let source = std::fs::read_to_string(test_path)?;
        let sgl = match loader.load_for_file(test_path, Some(&source), &NoCancellation)? {
            Some(sgl) => sgl,
            None => {
                if self.show_ignored {
                    println!("{} {}", "⦵".dimmed(), test_path.display());
                }
                return Ok(0);
            }
        };
        let default_fragment_path = test_path.strip_prefix(test_root).unwrap();
        let mut test = Test::from_source(&test_path, &source, default_fragment_path)?;
        self.load_builtins_into(sgl, &mut test.graph)
            .with_context(|| format!("Loading builtins into {}", test_path.display()))?;
        let mut globals = Variables::new();
        for test_fragment in &test.fragments {
            let fragment_path = Path::new(test.graph[test_fragment.file].name()).to_path_buf();
            if test_path.extension() != fragment_path.extension() {
                return Err(anyhow!(
                    "Test fragment {} has different file extension than test file {}",
                    fragment_path.display(),
                    test_path.display()
                ));
            }
            globals.clear();
            test_fragment.add_globals_to(&mut globals);
            self.build_fragment_stack_graph_into(
                &fragment_path,
                sgl,
                test_fragment.file,
                &test_fragment.source,
                &globals,
                &mut test.graph,
            )?;
        }
        let result = test.run(&NoCancellation)?;
        let success = self.handle_result(test_path, &result)?;
        if self.output_mode.test(!success) {
            let files = test.fragments.iter().map(|f| f.file).collect::<Vec<_>>();
            self.save_output(
                test_root,
                test_path,
                &test.graph,
                &mut test.paths,
                &|_: &StackGraph, h: &Handle<File>| files.contains(h),
                success,
            )?;
        }
        Ok(result.failure_count())
    }

    fn load_builtins_into(
        &self,
        sgl: &mut StackGraphLanguage,
        graph: &mut StackGraph,
    ) -> anyhow::Result<()> {
        if let Err(h) = graph.add_from_graph(sgl.builtins()) {
            return Err(anyhow!("Duplicate builtin file {}", &graph[h]));
        }
        Ok(())
    }

    fn build_fragment_stack_graph_into(
        &self,
        test_path: &Path,
        sgl: &mut StackGraphLanguage,
        file: Handle<File>,
        source: &str,
        globals: &Variables,
        graph: &mut StackGraph,
    ) -> anyhow::Result<()> {
        match sgl.build_stack_graph_into(graph, file, source, globals, &NoCancellation) {
            Err(LoadError::ParseErrors(parse_errors)) => {
                Err(map_parse_errors(test_path, &parse_errors, source))
            }
            Err(e) => Err(e.into()),
            Ok(_) => Ok(()),
        }
    }

    fn handle_result(&self, test_path: &Path, result: &TestResult) -> anyhow::Result<bool> {
        let success = result.failure_count() == 0;
        if !success || !self.hide_passing {
            println!(
                "{} {}: {}/{} assertions",
                if success { "✓".green() } else { "✗".red() },
                test_path.display(),
                result.success_count(),
                result.count()
            );
        }
        if !success && !self.hide_failure_errors {
            for failure in result.failures_iter() {
                println!("  {}", failure);
            }
        }
        Ok(success)
    }

    fn save_output(
        &self,
        test_root: &Path,
        test_path: &Path,
        graph: &StackGraph,
        paths: &mut Paths,
        filter: &dyn Filter,
        success: bool,
    ) -> anyhow::Result<()> {
        if let Some(path) = self
            .save_graph
            .as_ref()
            .map(|spec| spec.format(test_root, test_path))
        {
            self.save_graph(&path, &graph, filter)?;
            if !success || !self.hide_passing {
                println!("  Graph: {}", path.display());
            }
        }
        if let Some(path) = self
            .save_paths
            .as_ref()
            .map(|spec| spec.format(test_root, test_path))
        {
            self.save_paths(&path, paths, graph, filter)?;
            if !success || !self.hide_passing {
                println!("  Paths: {}", path.display());
            }
        }
        if let Some(path) = self
            .save_visualization
            .as_ref()
            .map(|spec| spec.format(test_root, test_path))
        {
            self.save_visualization(&path, paths, graph, filter, &test_path)?;
            if !success || !self.hide_passing {
                println!("  Visualization: {}", path.display());
            }
        }
        Ok(())
    }

    fn save_graph(
        &self,
        path: &Path,
        graph: &StackGraph,
        filter: &dyn Filter,
    ) -> anyhow::Result<()> {
        let json = graph.to_json(filter).to_string_pretty()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, json)
            .with_context(|| format!("Unable to write graph {}", path.display()))?;
        Ok(())
    }

    fn save_paths(
        &self,
        path: &Path,
        paths: &mut Paths,
        graph: &StackGraph,
        filter: &dyn Filter,
    ) -> anyhow::Result<()> {
        let json = paths.to_json(graph, filter).to_string_pretty()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, json)
            .with_context(|| format!("Unable to write graph {}", path.display()))?;
        Ok(())
    }

    fn save_visualization(
        &self,
        path: &Path,
        paths: &mut Paths,
        graph: &StackGraph,
        filter: &dyn Filter,
        test_path: &Path,
    ) -> anyhow::Result<()> {
        let html = graph.to_html_string(&format!("{}", test_path.display()), paths, filter)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, html)
            .with_context(|| format!("Unable to write graph {}", path.display()))?;
        Ok(())
    }
}

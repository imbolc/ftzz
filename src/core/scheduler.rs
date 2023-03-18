use std::{
    collections::VecDeque,
    io,
    num::{NonZeroU64, NonZeroUsize},
    ops::AddAssign,
    path::PathBuf,
    process::ExitCode,
    result,
};

use error_stack::{IntoReport, Result, ResultExt};
use rand_distr::Normal;
use tokio::task::{JoinError, JoinHandle};
use tracing::{event, span, Level, Span};

use crate::{
    core::{
        files::GeneratorTaskOutcome,
        tasks::{QueueErrors, QueueOutcome, TaskGenerator},
        truncatable_normal,
    },
    generator::Error,
    utils::{with_dir_name, FastPathBuf},
};

#[derive(Debug, Copy, Clone)]
pub struct GeneratorStats {
    pub files: u64,
    pub dirs: usize,
    pub bytes: u64,
}

impl AddAssign<&GeneratorTaskOutcome> for GeneratorStats {
    fn add_assign(
        &mut self,
        GeneratorTaskOutcome {
            files_generated,
            dirs_generated,
            bytes_generated,
            ..
        }: &GeneratorTaskOutcome,
    ) {
        self.files += files_generated;
        self.dirs += dirs_generated;
        self.bytes += bytes_generated;
    }
}

struct Scheduler<'a> {
    #[cfg(not(feature = "dry_run"))]
    tasks: &'a mut VecDeque<JoinHandle<Result<GeneratorTaskOutcome, io::Error>>>,
    #[cfg(feature = "dry_run")]
    tasks: &'a mut VecDeque<GeneratorTaskOutcome>,
    stats: &'a mut GeneratorStats,

    stack: Vec<Directory>,
    target_dir: FastPathBuf,

    cache: ObjectPool,
}

/// Given a directory
/// root/
/// ├── a/
/// ├── b/
/// └── c/
/// [`total_dirs`] is 3 and [`child_dir_counts`] contains 3 entries, each of
/// which specifies the number of directories to generate within `a`, `b`, and
/// `c` respectively.
struct Directory {
    total_dirs: usize,
    child_dir_counts: Vec<DirChild>,
}

struct DirChild {
    files: u64,
    dirs: usize,
}

struct ObjectPool {
    directories: Vec<Vec<DirChild>>,
    paths: Vec<FastPathBuf>,
    byte_counts: Vec<Vec<u64>>,
}

pub async fn run(
    root_dir: PathBuf,
    target_file_count: NonZeroU64,
    dirs_per_dir: f64,
    max_depth: usize,
    parallelism: NonZeroUsize,
    mut generator: impl TaskGenerator + Send,
) -> Result<GeneratorStats, Error> {
    // Minus 1 because VecDeque adds 1 and then rounds to a power of 2
    let mut tasks = VecDeque::with_capacity(parallelism.get().pow(2) - 1);
    let mut stats = GeneratorStats {
        files: 0,
        dirs: 0,
        bytes: 0,
    };

    let mut scheduler = Scheduler {
        stack: Vec::with_capacity(max_depth),
        #[cfg(unix)]
        target_dir: FastPathBuf::from(root_dir),
        #[cfg(not(unix))]
        target_dir: root_dir,

        cache: {
            let paths = Vec::with_capacity(tasks.capacity() / 2);
            ObjectPool {
                directories: Vec::with_capacity(max_depth),
                byte_counts: generator
                    .uses_byte_counts_pool()
                    .then(|| Vec::with_capacity(paths.capacity()))
                    .unwrap_or_default(),
                paths,
            }
        },

        tasks: &mut tasks,
        stats: &mut stats,
    };

    event!(
        Level::DEBUG,
        task_queue = scheduler.tasks.capacity(),
        object_pool.dirs = scheduler.cache.directories.capacity(),
        object_pool.paths = scheduler.cache.paths.capacity(),
        object_pool.file_sizes = scheduler.cache.byte_counts.capacity(),
        "Entry allocations"
    );

    schedule_root_dir(
        &mut generator,
        target_file_count,
        dirs_per_dir,
        max_depth,
        &mut scheduler,
    );

    let gen_span = span!(Level::TRACE, "dir_gen");
    while let Some(dir) = scheduler.stack.last_mut() {
        let Directory {
            total_dirs,
            ref mut child_dir_counts,
        } = *dir;
        let Some(DirChild {
                     files: target_file_count,
                     dirs: num_dirs_to_generate,
                 }) = child_dir_counts.pop() else {
            handle_directory_completion(&mut scheduler);
            continue;
        };

        let next_stack_dir = total_dirs - child_dir_counts.len();
        let is_completing = child_dir_counts.is_empty();

        if scheduler.tasks.len() + num_dirs_to_generate >= scheduler.tasks.capacity() {
            flush_tasks(&mut scheduler).await?;
        }

        let Ok(directory) = schedule_task(
            target_file_count,
            num_dirs_to_generate,
            dirs_per_dir,
            max_depth,
            &mut generator,
            &mut scheduler,
            &gen_span,
        ) else {
            break;
        };

        if let Some(directory) = directory {
            scheduler.stack.push(directory);
            with_dir_name(0, |s| scheduler.target_dir.push(s));
        } else if !is_completing {
            with_dir_name(next_stack_dir, |s| scheduler.target_dir.set_file_name(s));
        }
    }
    drop(gen_span);

    schedule_last_task(generator, scheduler);

    for task in tasks {
        #[cfg(not(feature = "dry_run"))]
        handle_task_result(task.await, &mut stats)?;
        #[cfg(feature = "dry_run")]
        handle_task_result(task, &mut stats)?;
    }

    Ok(stats)
}

async fn flush_tasks(scheduler: &mut Scheduler<'_>) -> Result<(), Error> {
    let Scheduler {
        ref mut tasks,
        ref mut stats,
        cache:
            ObjectPool {
                directories: _,
                paths: ref mut path_pool,
                byte_counts: ref mut byte_counts_pool,
            },
        ..
    } = *scheduler;

    event!(Level::TRACE, "Flushing pending task queue");
    for task in tasks.drain(..tasks.len() / 2) {
        #[cfg(not(feature = "dry_run"))]
        let outcome = handle_task_result(task.await, stats)?;
        #[cfg(feature = "dry_run")]
        let outcome = handle_task_result(task, stats)?;

        path_pool.push(outcome.pool_return_file);
        if let Some(mut vec) = outcome.pool_return_byte_counts {
            vec.clear();
            byte_counts_pool.push(vec);
        }
    }
    Ok(())
}

fn handle_task_result(
    #[cfg(not(feature = "dry_run"))] task_result: result::Result<
        Result<GeneratorTaskOutcome, io::Error>,
        JoinError,
    >,
    #[cfg(feature = "dry_run")] outcome: GeneratorTaskOutcome,
    stats: &mut GeneratorStats,
) -> Result<GeneratorTaskOutcome, Error> {
    #[cfg(not(feature = "dry_run"))]
    let outcome = task_result
        .into_report()
        .change_context(Error::TaskJoin)
        .attach(ExitCode::from(sysexits::ExitCode::Software))?
        .change_context(Error::Io)
        .attach(ExitCode::from(sysexits::ExitCode::IoErr))?;
    *stats += &outcome;
    Ok(outcome)
}

fn schedule_root_dir(
    generator: &mut impl TaskGenerator,
    target_file_count: NonZeroU64,
    dirs_per_dir: f64,
    max_depth: usize,
    scheduler: &mut Scheduler<'_>,
) {
    let Scheduler {
        ref mut tasks,
        stats: _,
        ref mut stack,
        ref target_dir,
        cache:
            ObjectPool {
                directories: _,
                paths: ref mut path_pool,
                byte_counts: ref mut byte_counts_pool,
            },
    } = *scheduler;

    match generator.queue_gen(
        &num_files_distr(target_file_count.get(), dirs_per_dir, max_depth),
        target_dir.clone(),
        max_depth > 0,
        byte_counts_pool,
    ) {
        Ok(QueueOutcome {
            task,
            num_files,
            num_dirs,
            done: _,
        }) => {
            tasks.push_back(task);
            if num_dirs > 0 {
                stack.push(Directory {
                    total_dirs: 1,
                    child_dir_counts: vec![DirChild {
                        files: next_target_file_count(target_file_count.get(), num_dirs, num_files),
                        dirs: num_dirs,
                    }],
                });
            }
        }
        Err(QueueErrors::NothingToDo(path)) => path_pool.push(path),
    }
}

fn schedule_task(
    target_file_count: u64,
    num_dirs_to_generate: usize,
    dirs_per_dir: f64,
    max_depth: usize,
    generator: &mut impl TaskGenerator,
    scheduler: &mut Scheduler<'_>,
    gen_span: &Span,
) -> result::Result<Option<Directory>, ()> {
    let Scheduler {
        ref mut tasks,
        stats: _,
        ref stack,
        ref target_dir,
        cache:
            ObjectPool {
                directories: ref mut dir_pool,
                paths: ref mut path_pool,
                byte_counts: ref mut byte_counts_pool,
            },
    } = *scheduler;

    let depth = stack.len();
    let gen_next_dirs = depth < max_depth;

    let mut next_dirs = dir_pool.pop().unwrap_or_default();
    debug_assert!(next_dirs.is_empty());
    if gen_next_dirs {
        // TODO figure out if we can bound this memory usage
        next_dirs.reserve(num_dirs_to_generate);
    }
    // Allocate a queue without VecDeque since we know the queue length will only
    // shrink. We want a queue so that the first task that is scheduled
    // is the directory we investigate first such that it will hopefully
    // have finished creating its directories (and thus minimize lock
    // contention).
    let raw_next_dirs = next_dirs.spare_capacity_mut();

    let num_files_distr = num_files_distr(target_file_count, dirs_per_dir, max_depth - depth);

    let span_guard = gen_span.enter();
    for i in 0..num_dirs_to_generate {
        let path = with_dir_name(i, |s| {
            let mut buf = path_pool.pop().unwrap_or_else(|| {
                // Space for inner, the path separator, name, and a NUL terminator
                FastPathBuf::with_capacity(target_dir.capacity() + 1 + s.len() + 1)
            });

            buf.clone_from(target_dir);
            buf.push(s);

            buf
        });

        let child =
            match generator.queue_gen(&num_files_distr, path, gen_next_dirs, byte_counts_pool) {
                Ok(QueueOutcome {
                    task,
                    num_files,
                    num_dirs,
                    done,
                }) => {
                    tasks.push_back(task);
                    if done {
                        return Err(());
                    }
                    DirChild {
                        files: next_target_file_count(target_file_count, num_dirs, num_files),
                        dirs: num_dirs,
                    }
                }
                Err(QueueErrors::NothingToDo(path)) => {
                    path_pool.push(path);
                    DirChild { files: 0, dirs: 0 }
                }
            };

        if gen_next_dirs {
            raw_next_dirs[num_dirs_to_generate - i - 1].write(child);
        }
    }
    drop(span_guard);

    if gen_next_dirs {
        unsafe {
            next_dirs.set_len(num_dirs_to_generate);
        }
        Ok(Some(Directory {
            total_dirs: num_dirs_to_generate,
            child_dir_counts: next_dirs,
        }))
    } else {
        dir_pool.push(next_dirs);
        Ok(None)
    }
}

fn schedule_last_task(mut generator: impl TaskGenerator, mut scheduler: Scheduler<'_>) {
    let Scheduler {
        ref mut tasks,
        stats: _,
        stack: _,
        target_dir,
        cache:
            ObjectPool {
                byte_counts: ref mut byte_counts_pool,
                ..
            },
    } = scheduler;

    if let Ok(QueueOutcome {
        task,
        num_files: _,
        num_dirs: _,
        done: _,
    }) = generator.maybe_queue_final_gen(target_dir, byte_counts_pool)
    {
        tasks.push_back(task);
    }
}

fn handle_directory_completion(scheduler: &mut Scheduler<'_>) {
    let Scheduler {
        tasks: _,
        stats: _,
        ref mut stack,
        ref mut target_dir,
        cache:
            ObjectPool {
                directories: ref mut directory_pool,
                ..
            },
    } = *scheduler;

    if let Some(Directory {
        total_dirs: _,
        child_dir_counts,
    }) = stack.pop()
    {
        directory_pool.push(child_dir_counts);
    }

    if let Some(Directory {
        total_dirs,
        child_dir_counts,
    }) = stack.last()
    {
        target_dir.pop();

        if !child_dir_counts.is_empty() {
            with_dir_name(*total_dirs - child_dir_counts.len(), |s| {
                target_dir.set_file_name(s);
            });
        }
    }
}

fn next_target_file_count(target_file_count: u64, dirs_created: usize, files_created: u64) -> u64 {
    let files = target_file_count.saturating_sub(files_created);
    files
        .checked_div(u64::try_from(dirs_created).unwrap_or(u64::MAX))
        .unwrap_or(files_created)
}

#[allow(clippy::cast_precision_loss)]
fn num_files_distr(
    target_file_count: u64,
    dirs_per_dir: f64,
    remaining_depth: usize,
) -> Normal<f64> {
    fn files_per_dir(total_files: u64, dirs_per_dir: f64, remaining_depth: usize) -> f64 {
        (total_files as f64) * dirs_per_dir.powf(-(remaining_depth as f64))
    }

    truncatable_normal(files_per_dir(
        target_file_count,
        dirs_per_dir,
        remaining_depth,
    ))
}

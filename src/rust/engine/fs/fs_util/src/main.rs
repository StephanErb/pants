// Enable all clippy lints except for many of the pedantic ones. It's a shame this needs to be copied and pasted across crates, but there doesn't appear to be a way to include inner attributes from a common source.
#![cfg_attr(
  feature = "cargo-clippy",
  deny(
    clippy,
    default_trait_access,
    expl_impl_clone_on_copy,
    if_not_else,
    needless_continue,
    single_match_else,
    unseparated_literal_suffix,
    used_underscore_binding
  )
)]
// It is often more clear to show that nothing is being moved.
#![cfg_attr(feature = "cargo-clippy", allow(match_ref_pats))]
// Subjective style.
#![cfg_attr(
  feature = "cargo-clippy",
  allow(len_without_is_empty, redundant_field_names)
)]
// Default isn't as big a deal as people seem to think it is.
#![cfg_attr(
  feature = "cargo-clippy",
  allow(new_without_default, new_without_default_derive)
)]
// Arc<Mutex> can be more clear than needing to grok Orderings:
#![cfg_attr(feature = "cargo-clippy", allow(mutex_atomic))]

#[macro_use]
extern crate boxfuture;
extern crate bytes;
#[macro_use(value_t)]
extern crate clap;
extern crate env_logger;
extern crate fs;
extern crate futures;
extern crate hashing;
extern crate parking_lot;
extern crate protobuf;
extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;

use boxfuture::{BoxFuture, Boxable};
use bytes::Bytes;
use clap::{App, Arg, SubCommand};
use fs::{GlobMatching, ResettablePool, Snapshot, Store, StoreFileByDigest, UploadSummary};
use futures::future::Future;
use hashing::{Digest, Fingerprint};
use parking_lot::Mutex;
use protobuf::Message;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::exit;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug)]
enum ExitCode {
  UnknownError = 1,
  NotFound = 2,
}

#[derive(Debug)]
struct ExitError(String, ExitCode);

impl From<String> for ExitError {
  fn from(s: String) -> Self {
    ExitError(s, ExitCode::UnknownError)
  }
}

#[derive(Serialize)]
struct SummaryWithDigest {
  digest: Digest,
  summary: Option<UploadSummary>,
}

fn main() {
  env_logger::init();

  match execute(
    &App::new("fs_util")
      .subcommand(
        SubCommand::with_name("file")
          .subcommand(
            SubCommand::with_name("cat")
              .about("Output the contents of a file by fingerprint.")
              .arg(Arg::with_name("fingerprint").required(true).takes_value(
                true,
              ))
              .arg(Arg::with_name("size_bytes").required(true).takes_value(
                true,
              )),
          )
          .subcommand(
            SubCommand::with_name("save")
              .about(
                "Ingest a file by path, which allows it to be used in Directories/Snapshots. \
Outputs a fingerprint of its contents and its size in bytes, separated by a space.",
              )
              .arg(Arg::with_name("path").required(true).takes_value(true))
              .arg(Arg::with_name("output-mode").long("output-mode").possible_values(&["json", "simple"]).default_value("simple").multiple(false).takes_value(true).help(
                "Set to manipulate the way a report is displayed."
              )),
          ),
      )
      .subcommand(
        SubCommand::with_name("directory")
          .subcommand(
            SubCommand::with_name("materialize")
              .about(
                "Materialize a directory by fingerprint to the filesystem. \
Destination must not exist before this command is run.",
              )
              .arg(Arg::with_name("fingerprint").required(true).takes_value(
                true,
              ))
              .arg(Arg::with_name("size_bytes").required(true).takes_value(
                true,
              ))
              .arg(Arg::with_name("destination").required(true).takes_value(
                true,
              )),
          )
          .subcommand(
            SubCommand::with_name("save")
              .about(
                "Ingest a directory recursively. Saves all files found therein and saves Directory \
protos for each directory found. Outputs a fingerprint of the canonical top-level Directory proto \
and the size of the serialized proto in bytes, separated by a space.",
              )
              .arg(
                Arg::with_name("globs")
                  .required(true)
                  .takes_value(true)
                  .multiple(true)
                  .help(
                    "globs matching the files and directories which should be included in the \
directory, relative to the root.",
                  ),
              )
                .arg(Arg::with_name("root").long("root").required(true).takes_value(true).help(
                  "Root under which the globs live. The Directory proto produced will be relative \
to this directory.",
            ))
                .arg(Arg::with_name("output-mode").long("output-mode").possible_values(&["json", "simple"]).default_value("simple").multiple(false).takes_value(true).help(
                  "Set to manipulate the way a report is displayed."
                )),
          )
          .subcommand(
            SubCommand::with_name("cat-proto")
              .about(
                "Output the bytes of a serialized Directory proto addressed by fingerprint.",
              )
              .arg(
                Arg::with_name("output-format")
                  .long("output-format")
                  .takes_value(true)
                  .default_value("binary")
                  .possible_values(&["binary", "recursive-file-list", "text"]),
              )
              .arg(Arg::with_name("fingerprint").required(true).takes_value(
                true,
              ))
              .arg(Arg::with_name("size_bytes").required(true).takes_value(
                true,
              )),
          ),
      )
      .subcommand(
        SubCommand::with_name("cat")
          .about(
            "Output the contents of a file or Directory proto addressed by fingerprint.",
          )
          .arg(Arg::with_name("fingerprint").required(true).takes_value(
            true,
          ))
          .arg(Arg::with_name("size_bytes").required(true).takes_value(
            true,
          )),
      )
        .subcommand(
          SubCommand::with_name("gc")
              .about("Garbage collect the on-disk store. Note that after running this command, any processes with an open store (e.g. a pantsd) may need to re-initialize their store.")
              .arg(
                Arg::with_name("target-size-bytes")
                    .takes_value(true)
                    .long("target-size-bytes")
                    .required(true),
              )
        )
      .arg(
        Arg::with_name("local-store-path")
          .takes_value(true)
          .long("local-store-path")
          .required(false),
      )
        .arg(
          Arg::with_name("server-address")
              .takes_value(true)
              .long("server-address")
              .required(false)
        )
        .arg(
          Arg::with_name("root-ca-cert-file")
              .help("Path to file containing root certificate authority certificates. If not set, TLS will not be used when connecting to the remote.")
              .takes_value(true)
              .long("root-ca-cert-file")
              .required(false)
        )
        .arg(
          Arg::with_name("oauth-bearer-token-file")
              .help("Path to file containing oauth bearer token. If not set, no authorization will be provided to remote servers.")
              .takes_value(true)
              .long("oauth-bearer-token-file")
              .required(false)
        )
        .arg(Arg::with_name("remote-instance-name")
            .takes_value(true)
                 .long("remote-instance-name")
                 .required(false))
        .arg(
          Arg::with_name("chunk-bytes")
              .help("Number of bytes to include per-chunk when uploading bytes. grpc imposes a hard message-size limit of around 4MB.")
              .takes_value(true)
              .long("chunk-bytes")
              .required(false)
              .default_value(&format!("{}", 3 * 1024 * 1024))
        )
      .get_matches(),
  ) {
    Ok(_) => {}
    Err(err) => {
      eprintln!("{}", err.0);
      exit(err.1 as i32)
    }
  };
}

fn execute(top_match: &clap::ArgMatches) -> Result<(), ExitError> {
  let store_dir = top_match
    .value_of("local-store-path")
    .map(PathBuf::from)
    .unwrap_or_else(Store::default_path);
  let pool = Arc::new(ResettablePool::new("fsutil-pool-".to_string()));
  let (store, store_has_remote) = {
    let (store_result, store_has_remote) = match top_match.value_of("server-address") {
      Some(cas_address) => {
        let chunk_size =
          value_t!(top_match.value_of("chunk-bytes"), usize).expect("Bad chunk-bytes flag");

        let root_ca_certs = if let Some(path) = top_match.value_of("root-ca-cert-file") {
          Some(
            std::fs::read(path)
              .map_err(|err| format!("Error reading root CA certs file {}: {}", path, err))?,
          )
        } else {
          None
        };

        let oauth_bearer_token =
          if let Some(path) = top_match.value_of("oauth-bearer-token-file") {
            Some(std::fs::read_to_string(path).map_err(|err| {
              format!("Error reading oauth bearer token from {:?}: {}", path, err)
            })?)
          } else {
            None
          };

        (
          Store::with_remote(
            &store_dir,
            pool.clone(),
            cas_address,
            top_match
              .value_of("remote-instance-name")
              .map(str::to_owned),
            root_ca_certs,
            oauth_bearer_token,
            1,
            chunk_size,
            // This deadline is really only in place because otherwise DNS failures
            // leave this hanging forever.
            //
            // Make fs_util have a very long deadline (because it's not configurable,
            // like it is inside pants) until we switch to Tower (where we can more
            // carefully control specific components of timeouts).
            //
            // See https://github.com/pantsbuild/pants/pull/6433 for more context.
            Duration::from_secs(30 * 60),
          ),
          true,
        )
      }
      None => (Store::local_only(&store_dir, pool.clone()), false),
    };
    let store = store_result.map_err(|e| {
      format!(
        "Failed to open/create store for directory {:?}: {}",
        store_dir, e
      )
    })?;
    (store, store_has_remote)
  };

  match top_match.subcommand() {
    ("file", Some(sub_match)) => {
      match sub_match.subcommand() {
        ("cat", Some(args)) => {
          let fingerprint = Fingerprint::from_hex_string(args.value_of("fingerprint").unwrap())?;
          let size_bytes = args
            .value_of("size_bytes")
            .unwrap()
            .parse::<usize>()
            .expect("size_bytes must be a non-negative number");
          let digest = Digest(fingerprint, size_bytes);
          let write_result = store
            .load_file_bytes_with(digest, |bytes| io::stdout().write_all(&bytes).unwrap())
            .wait()?;
          write_result.ok_or_else(|| {
            ExitError(
              format!("File with digest {:?} not found", digest),
              ExitCode::NotFound,
            )
          })
        }
        ("save", Some(args)) => {
          let path = PathBuf::from(args.value_of("path").unwrap());
          // Canonicalize path to guarantee that a relative path has a parent.
          let posix_fs = make_posix_fs(
            path
              .canonicalize()
              .map_err(|e| format!("Error canonicalizing path {:?}: {:?}", path, e))?
              .parent()
              .ok_or_else(|| format!("File being saved must have parent but {:?} did not", path))?,
            pool,
          );
          let file = posix_fs
            .stat(PathBuf::from(path.file_name().unwrap()))
            .unwrap();
          match file {
            fs::Stat::File(f) => {
              let digest = fs::OneOffStoreFileByDigest::new(store.clone(), Arc::new(posix_fs))
                .store_by_digest(f)
                .wait()
                .unwrap();

              let report = ensure_uploaded_to_remote(&store, store_has_remote, digest);
              print_upload_summary(args.value_of("output-mode"), &report);

              Ok(())
            }
            o => Err(
              format!(
                "Tried to save file {:?} but it was not a file, was a {:?}",
                path, o
              ).into(),
            ),
          }
        }
        (_, _) => unimplemented!(),
      }
    }
    ("directory", Some(sub_match)) => match sub_match.subcommand() {
      ("materialize", Some(args)) => {
        let destination = PathBuf::from(args.value_of("destination").unwrap());
        let fingerprint = Fingerprint::from_hex_string(args.value_of("fingerprint").unwrap())?;
        let size_bytes = args
          .value_of("size_bytes")
          .unwrap()
          .parse::<usize>()
          .expect("size_bytes must be a non-negative number");
        let digest = Digest(fingerprint, size_bytes);
        store
          .materialize_directory(destination, digest)
          .wait()
          .map_err(|err| {
            if err.contains("not found") {
              ExitError(err, ExitCode::NotFound)
            } else {
              err.into()
            }
          })
      }
      ("save", Some(args)) => {
        let posix_fs = Arc::new(make_posix_fs(args.value_of("root").unwrap(), pool));
        let store_copy = store.clone();
        let digest = posix_fs
          .expand(fs::PathGlobs::create(
            &args
              .values_of("globs")
              .unwrap()
              .map(|s| s.to_string())
              .collect::<Vec<String>>(),
            &[],
            // By using `Ignore`, we assume all elements of the globs will definitely expand to
            // something here, or we don't care. Is that a valid assumption?
            fs::StrictGlobMatching::Ignore,
            fs::GlobExpansionConjunction::AllMatch,
          )?).map_err(|e| format!("Error expanding globs: {:?}", e))
          .and_then(move |paths| {
            Snapshot::from_path_stats(
              store_copy.clone(),
              &fs::OneOffStoreFileByDigest::new(store_copy, posix_fs),
              paths,
            )
          }).map(|snapshot| snapshot.digest)
          .wait()?;

        let report = ensure_uploaded_to_remote(&store, store_has_remote, digest);
        print_upload_summary(args.value_of("output-mode"), &report);

        Ok(())
      }
      ("cat-proto", Some(args)) => {
        let fingerprint = Fingerprint::from_hex_string(args.value_of("fingerprint").unwrap())?;
        let size_bytes = args
          .value_of("size_bytes")
          .unwrap()
          .parse::<usize>()
          .expect("size_bytes must be a non-negative number");
        let digest = Digest(fingerprint, size_bytes);
        let proto_bytes = match args.value_of("output-format").unwrap() {
          "binary" => store
            .load_directory(digest)
            .wait()
            .map(|maybe_d| maybe_d.map(|d| d.write_to_bytes().unwrap())),
          "text" => store
            .load_directory(digest)
            .wait()
            .map(|maybe_p| maybe_p.map(|p| format!("{:?}\n", p).as_bytes().to_vec())),
          "recursive-file-list" => expand_files(store, digest).map(|maybe_v| {
            maybe_v
              .map(|v| {
                v.into_iter()
                  .map(|f| format!("{}\n", f))
                  .collect::<Vec<String>>()
                  .join("")
              }).map(|s| s.into_bytes())
          }),
          format => Err(format!(
            "Unexpected value of --output-format arg: {}",
            format
          )),
        }?;
        match proto_bytes {
          Some(bytes) => {
            io::stdout().write_all(&bytes).unwrap();
            Ok(())
          }
          None => Err(ExitError(
            format!("Directory with digest {:?} not found", digest),
            ExitCode::NotFound,
          )),
        }
      }
      (_, _) => unimplemented!(),
    },
    ("cat", Some(args)) => {
      let fingerprint = Fingerprint::from_hex_string(args.value_of("fingerprint").unwrap())?;
      let size_bytes = args
        .value_of("size_bytes")
        .unwrap()
        .parse::<usize>()
        .expect("size_bytes must be a non-negative number");
      let digest = Digest(fingerprint, size_bytes);
      let v = match store.load_file_bytes_with(digest, |bytes| bytes).wait()? {
        None => store
          .load_directory(digest)
          .map(|maybe_dir| {
            maybe_dir.map(|dir| {
              Bytes::from(
                dir
                  .write_to_bytes()
                  .expect("Error serializing Directory proto"),
              )
            })
          }).wait()?,
        some => some,
      };
      match v {
        Some(bytes) => {
          io::stdout().write_all(&bytes).unwrap();
          Ok(())
        }
        None => Err(ExitError(
          format!("Digest {:?} not found", digest),
          ExitCode::NotFound,
        )),
      }
    }
    ("gc", Some(args)) => {
      let target_size_bytes = value_t!(args.value_of("target-size-bytes"), usize)
        .expect("--target-size-bytes must be passed as a non-negative integer");
      store.garbage_collect(target_size_bytes, fs::ShrinkBehavior::Compact)?;
      Ok(())
    }

    (_, _) => unimplemented!(),
  }
}

fn expand_files(store: Store, digest: Digest) -> Result<Option<Vec<String>>, String> {
  let files = Arc::new(Mutex::new(Vec::new()));
  expand_files_helper(store, digest, String::new(), files.clone())
    .wait()
    .map(|maybe| {
      maybe.map(|()| {
        let mut v = Arc::try_unwrap(files).unwrap().into_inner();
        v.sort();
        v
      })
    })
}

fn expand_files_helper(
  store: Store,
  digest: Digest,
  prefix: String,
  files: Arc<Mutex<Vec<String>>>,
) -> BoxFuture<Option<()>, String> {
  store
    .load_directory(digest)
    .and_then(|maybe_dir| match maybe_dir {
      Some(dir) => {
        {
          let mut files_unlocked = files.lock();
          for file in dir.get_files() {
            files_unlocked.push(format!("{}{}", prefix, file.name));
          }
        }
        futures::future::join_all(
          dir
            .get_directories()
            .into_iter()
            .map(move |dir| {
              let digest: Result<Digest, String> = dir.get_digest().into();
              expand_files_helper(
                store.clone(),
                try_future!(digest),
                format!("{}{}/", prefix, dir.name),
                files.clone(),
              )
            }).collect::<Vec<_>>(),
        ).map(|_| Some(()))
        .to_boxed()
      }
      None => futures::future::ok(None).to_boxed(),
    }).to_boxed()
}

fn make_posix_fs<P: AsRef<Path>>(root: P, pool: Arc<ResettablePool>) -> fs::PosixFS {
  fs::PosixFS::new(&root, pool, &[]).unwrap()
}

fn ensure_uploaded_to_remote(
  store: &Store,
  store_has_remote: bool,
  digest: Digest,
) -> SummaryWithDigest {
  let summary = if store_has_remote {
    Some(
      store
        .ensure_remote_has_recursive(vec![digest])
        .wait()
        .unwrap(),
    )
  } else {
    None
  };
  SummaryWithDigest { digest, summary }
}

fn print_upload_summary(mode: Option<&str>, report: &SummaryWithDigest) {
  match mode {
    Some("json") => println!("{}", serde_json::to_string_pretty(&report).unwrap()),
    Some("simple") => println!("{} {}", report.digest.0, report.digest.1),
    // This should never be reached, as clap should error with unknown formats.
    _ => eprintln!("Unknown summary format."),
  };
}

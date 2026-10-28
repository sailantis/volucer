//! `include`: one configuration read from several files.
//!
//! What is asserted here is the precedence and the refusals, because those are the two things an
//! operator cannot see by reading one file: which of two files holding the same key wins, and what
//! a mistake in the graph of includes reports.

use super::*;

/// A directory that removes itself, named after the test so two tests cannot collide.
struct Fixture(PathBuf);

impl Fixture {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("volucer-include-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create the fixture");
        Fixture(path)
    }

    fn write(&self, relative: &str, text: &str) {
        let path = self.0.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create the parent");
        }
        std::fs::write(&path, text).expect("write the fixture");
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.0.join(relative)
    }

    fn create_directory(&self, relative: &str) {
        std::fs::create_dir_all(self.0.join(relative)).expect("create the directory");
    }

    /// Load `relative` as the main file and return the configuration, with the files it read.
    fn load(&self, relative: &str) -> Result<Loaded> {
        Config::load_from_path(&self.path(relative))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A file and what it includes both set a key: **the file wins**, and the include fills the rest.
///
/// The rule asserted in both directions from one fixture, because they are one decision: an
/// including file is authoritative and an included one is a base. Getting only the first half
/// would pass with a merge that dropped the include entirely.
#[test]
fn the_including_file_wins_over_what_it_includes() {
    let fixture = Fixture::new("precedence");
    fixture.write(
        "base.toml",
        "[server]\nhttp = \"127.0.0.1:1111\"\nfastcgi = \"127.0.0.1:2222\"\n",
    );
    fixture.write(
        "volucer.toml",
        "include = [\"base.toml\"]\n\n[server]\nhttp = \"127.0.0.1:8080\"\n",
    );

    let loaded = fixture
        .load("volucer.toml")
        .expect("the configuration loads");
    let config = Config::from_table(loaded.table).expect("the merged table deserializes");

    assert_eq!(
        config.server.http.as_deref(),
        Some("127.0.0.1:8080"),
        "the including file's value must survive"
    );
    assert_eq!(
        config.server.fastcgi.as_deref(),
        Some("127.0.0.1:2222"),
        "and the included file must still contribute what the including file does not set"
    );
    assert_eq!(loaded.included, vec![fixture.path("base.toml")]);
}

/// The directives are gone from the table, so nothing downstream can see a key the model has no
/// field for — and `deny_unknown_fields` would otherwise refuse every configuration that has one.
#[test]
fn the_directives_do_not_reach_the_deserializer() {
    let fixture = Fixture::new("removed");
    fixture.write("base.toml", "");
    fixture.write("conf.d/50-server.toml", "");
    fixture.write(
        "volucer.toml",
        "include = [\"base.toml\", \"conf.d\"]\n[server]\nhttp = \"127.0.0.1:8080\"\n",
    );

    let config = fixture
        .load("volucer.toml")
        .expect("the configuration loads");
    assert!(!config.table.contains_key("include"));
}

/// A directory's files are read in the **byte order of their names**, which is what makes the
/// numeric prefixes MariaDB's packaged files carry work here too.
///
/// `50-server.toml` wins over `10-base.toml`; `Z.toml` is read after both because uppercase sorts
/// before lowercase in byte order — asserted because "sorted" is ambiguous and this is the answer.
#[test]
fn a_directory_is_read_in_byte_order() {
    let fixture = Fixture::new("byte-order");
    fixture.write(
        "conf.d/10-base.toml",
        "[server]\nhttp = \"127.0.0.1:1111\"\n",
    );
    fixture.write(
        "conf.d/50-server.toml",
        "[server]\nhttp = \"127.0.0.1:5050\"\n",
    );
    fixture.write("conf.d/Z.toml", "[server]\nfastcgi = \"127.0.0.1:9000\"\n");
    fixture.write("volucer.toml", "include = [\"conf.d\"]\n");

    let loaded = fixture
        .load("volucer.toml")
        .expect("the configuration loads");
    let config = Config::from_table(loaded.table).expect("the merged table deserializes");

    assert_eq!(config.server.http.as_deref(), Some("127.0.0.1:5050"));
    assert_eq!(config.server.fastcgi.as_deref(), Some("127.0.0.1:9000"));
    assert_eq!(
        loaded.included,
        vec![
            fixture.path("conf.d/10-base.toml"),
            fixture.path("conf.d/50-server.toml"),
            fixture.path("conf.d/Z.toml"),
        ]
    );
}

/// Entries are read **in the order written**, whatever kind each one is: naming a file after a
/// directory puts it after that directory's drop-ins.
///
/// This is what one list buys over MariaDB's two directives. `!include` and `!includedir` cannot
/// be interleaved — the file reads as "all the files, then all the directories" or not at all — so
/// their precedence between the two kinds has to be learnt. Here the order on the page is the
/// order of the merge, with no second rule.
#[test]
fn entries_are_read_in_the_order_written() {
    let fixture = Fixture::new("order");
    fixture.write("base.toml", "[server]\nhttp = \"127.0.0.1:1111\"\n");
    fixture.write(
        "conf.d/50-server.toml",
        "[server]\nhttp = \"127.0.0.1:5050\"\n",
    );
    fixture.write("late.toml", "[server]\nhttp = \"127.0.0.1:9999\"\n");

    // The file last, so it wins — and the reverse order, where the directory wins.
    fixture.write(
        "file-last.toml",
        "include = [\"base.toml\", \"conf.d\", \"late.toml\"]\n",
    );
    fixture.write("dir-last.toml", "include = [\"late.toml\", \"conf.d\"]\n");

    let file_last = fixture
        .load("file-last.toml")
        .map(|loaded| Config::from_table(loaded.table))
        .expect("the configuration loads")
        .expect("the merged table deserializes");
    assert_eq!(
        file_last.server.http.as_deref(),
        Some("127.0.0.1:9999"),
        "a file named after the directory is read after it"
    );

    let dir_last = fixture
        .load("dir-last.toml")
        .map(|loaded| Config::from_table(loaded.table))
        .expect("the configuration loads")
        .expect("the merged table deserializes");
    assert_eq!(
        dir_last.server.http.as_deref(),
        Some("127.0.0.1:5050"),
        "and a directory named after the file is read after it"
    );
}

/// Only `.toml` files are read: a README or an editor's backup in `conf.d` must not be a
/// configuration, and a subdirectory must not be read as one.
#[test]
fn only_toml_files_are_read_from_a_directory() {
    let fixture = Fixture::new("filter");
    fixture.write(
        "conf.d/50-server.toml",
        "[server]\nhttp = \"127.0.0.1:5050\"\n",
    );
    fixture.write("conf.d/README.md", "not a configuration\n");
    fixture.write(
        "conf.d/50-server.toml.bak",
        "[server]\nhttp = \"0.0.0.0:1\"\n",
    );
    fixture.write(
        "conf.d/nested/99-late.toml",
        "[server]\nfastcgi = \"127.0.0.1:1\"\n",
    );
    fixture.write("volucer.toml", "include = [\"conf.d\"]\n");

    let loaded = fixture
        .load("volucer.toml")
        .expect("the configuration loads");
    assert_eq!(loaded.included, vec![fixture.path("conf.d/50-server.toml")]);
}

/// Two files that both list a route: the array is **replaced**, not concatenated.
///
/// There is no key to match two array elements on, so a merge would have to invent one. Replacing
/// is the answer that does not depend on which file happened to be read first.
#[test]
fn an_array_is_replaced_rather_than_concatenated() {
    let fixture = Fixture::new("array");
    fixture.write(
        "base.toml",
        "[server]\nhttp = \"127.0.0.1:8080\"\n[[server.routes]]\npath_prefix = \"/a\"\nbackend = \"x\"\n",
    );
    fixture.write(
        "volucer.toml",
        "include = [\"base.toml\"]\n[[server.routes]]\npath_prefix = \"/b\"\nbackend = \"y\"\n",
    );

    let config = fixture
        .load("volucer.toml")
        .map(|loaded| Config::from_table(loaded.table))
        .expect("the configuration loads")
        .expect("the merged table deserializes");
    let prefixes: Vec<Option<&str>> = config
        .server
        .routes
        .iter()
        .map(|route| route.path_prefix.as_deref())
        .collect();
    assert_eq!(prefixes, vec![Some("/b")]);
}

/// A file reached through another include is still `${...}`-expanded — and a reference that is
/// unset names **the included file**, not the one on the command line.
///
/// The path is the point of the test: an error naming the main file would send an operator to a
/// line that does not contain the mistake.
#[test]
fn an_unset_variable_in_an_included_file_names_that_file() {
    let fixture = Fixture::new("expansion");
    fixture.write(
        "base.toml",
        "[server]\nhttp = \"127.0.0.1:${VOLUCER_TEST_MISSING_VARIABLE}\"\n",
    );
    fixture.write("volucer.toml", "include = [\"base.toml\"]\n");

    let error = fixture
        .load("volucer.toml")
        .expect_err("an unset variable must be an error");
    let message = error.to_string();
    assert!(
        message.contains("base.toml"),
        "the message must name the included file: {message}"
    );
}

/// A file that includes itself is refused, and the message names the cycle rather than reporting
/// a stack overflow or reading until the depth limit is hit.
#[test]
fn a_cycle_is_refused_by_name() {
    let fixture = Fixture::new("cycle");
    fixture.write("volucer.toml", "include = [\"volucer.toml\"]\n");

    let error = fixture
        .load("volucer.toml")
        .expect_err("a cycle must be refused");
    let message = error.to_string();
    assert!(message.contains("returns to a file"), "{message}");
}

/// A cycle through two files is refused too: the check is on the path, not on one file.
#[test]
fn an_indirect_cycle_is_refused() {
    let fixture = Fixture::new("indirect-cycle");
    fixture.write("a.toml", "include = [\"b.toml\"]\n");
    fixture.write("b.toml", "include = [\"a.toml\"]\n");

    let error = fixture.load("a.toml").expect_err("a cycle must be refused");
    assert!(error.to_string().contains("returns to a file"), "{error}");
}

/// The same file written two ways is one file: `conf.d/../base.toml` closes a cycle through
/// `base.toml` just as the direct spelling does.
#[test]
fn a_cycle_through_a_different_spelling_is_refused() {
    let fixture = Fixture::new("canonical-cycle");
    fixture.create_directory("conf.d");
    fixture.write("base.toml", "include = [\"conf.d/../base.toml\"]\n");

    let error = fixture
        .load("base.toml")
        .expect_err("the same file by another path is still a cycle");
    assert!(error.to_string().contains("returns to a file"), "{error}");
}

/// A chain longer than the limit is refused, and the message names the chain so the operator can
/// see which file to flatten.
#[test]
fn a_chain_deep_hits_the_limit_and_says_so() {
    let fixture = Fixture::new("depth");
    for index in 0..12 {
        let next = if index == 11 {
            String::new()
        } else {
            format!("include = [\"{}.toml\"]\n", index + 1)
        };
        fixture.write(&format!("{index}.toml"), &next);
    }

    let error = fixture.load("0.toml").expect_err("the chain is too deep");
    let message = error.to_string();
    assert!(message.contains("files deep"), "{message}");
}

/// A directive written **after a table header** is read as the directive, not as a key of that
/// table.
///
/// This is the behaviour MariaDB's option files have for free — their `!include` is a line, and
/// their own documentation writes it inside `[mariadb]` — and that TOML cannot express with a bare
/// key, because a `[table]` header *is* a scope. Extracting the key from wherever it sits before the
/// merge restores it: position says nothing about meaning.
///
/// It is safe because no table in the model has an `include` field, so the name cannot be a real
/// setting. A key that used to be `unknown field 'include'` is now the directive it reads as.
#[test]
fn a_directive_after_a_table_header_is_still_the_directive() {
    let fixture = Fixture::new("misplaced");
    fixture.write("base.toml", "[server]\nfastcgi = \"127.0.0.1:2222\"\n");
    fixture.write(
        "volucer.toml",
        "[server]\nhttp = \"127.0.0.1:8080\"\ninclude = [\"base.toml\"]\n",
    );

    let loaded = fixture
        .load("volucer.toml")
        .expect("a directive inside a table is still read");
    assert_eq!(loaded.included, vec![fixture.path("base.toml")]);
    let config = Config::from_table(loaded.table).expect("the merged table deserializes");
    assert_eq!(
        config.server.fastcgi.as_deref(),
        Some("127.0.0.1:2222"),
        "the included file must be merged"
    );
    assert_eq!(config.server.http.as_deref(), Some("127.0.0.1:8080"));
}

/// The end-of-file shape works with a **bare key** too: after the last table, it is still read.
///
/// This is the case that once made a `[include]` table necessary — TOML has no way to return to the
/// root table. With the key extracted before the merge there is nothing to work around, and the
/// position in the file says nothing about the meaning.
#[test]
fn a_directive_at_the_end_of_the_file_is_read() {
    let fixture = Fixture::new("at-end");
    fixture.write(
        "conf.d/50-server.toml",
        "[server]\nfastcgi = \"127.0.0.1:2222\"\n",
    );
    fixture.write(
        "volucer.toml",
        "[server]\nhttp = \"127.0.0.1:8080\"\n\n\
         [backends.app]\nkind = \"worker\"\ncommand = \"php worker.php\"\n\n\
         include = [\"conf.d\"]\n",
    );

    let loaded = fixture
        .load("volucer.toml")
        .expect("a directive at the end of the file is read");
    let config = Config::from_table(loaded.table).expect("the merged table deserializes");
    assert_eq!(config.server.fastcgi.as_deref(), Some("127.0.0.1:2222"));
    assert_eq!(
        config.backends["app"].kind(),
        "worker",
        "and the table it was written under is untouched"
    );
}

/// A key that merely *starts with* `include` is not mistaken for a misplaced directive.
#[test]
fn an_ordinary_key_named_like_a_directive_is_left_alone() {
    let fixture = Fixture::new("lookalike");
    fixture.write(
        "volucer.toml",
        "[server]\nhttp = \"127.0.0.1:8080\"\nincluded_by = \"nobody\"\n",
    );

    // `included_by` is an unknown field, which is the strictness working — but it is `serde`'s
    // refusal about *that* key, not this module's refusal about a misplaced include.
    let error = fixture
        .load("volucer.toml")
        .map(|loaded| Config::from_table(loaded.table))
        .expect("the file loads")
        .expect_err("`included_by` is not a server key");
    let message = error.to_string();
    assert!(
        !message.contains("belongs to the table above it"),
        "a lookalike key must not be reported as a misplaced directive: {message}"
    );
}

/// MariaDB's second directive is refused **by name**, with the list that replaces it.
///
/// An operator arriving from `my.cnf` writes the key they know. `deny_unknown_fields` would say
/// "unknown field `include_dir`", which is true and useless: the key is not unknown, it is spelled
/// differently here, and the message is the one place that can say so.
#[test]
fn include_dir_is_refused_in_favour_of_the_list() {
    let fixture = Fixture::new("include-dir");
    fixture.write("volucer.toml", "include_dir = \"conf.d\"\n");

    let error = fixture
        .load("volucer.toml")
        .expect_err("include_dir is not a key");
    let message = error.to_string();
    assert!(
        message.contains("include = [\"conf.d\"]"),
        "the message must show the spelling that works: {message}"
    );
    assert!(
        message.contains("include = [\"conf.d\", \"sites.d\"]"),
        "and that several directories are one list: {message}"
    );
}

/// The `[include]` table is refused too, and for the reason it once existed: it was a way to reach
/// the end of the file, which the bare key now does on its own.
///
/// Named rather than silently accepted, because it was documented and someone may have copied it.
#[test]
fn the_include_table_is_refused_in_favour_of_the_list() {
    let fixture = Fixture::new("table-form");
    fixture.write(
        "conf.d/50-static.toml",
        "[server.static_files]\nenabled = true\n",
    );
    fixture.write(
        "volucer.toml",
        "[server]\nhttp = \"127.0.0.1:8080\"\n\n\
         [include]\nfiles = [\"base.toml\"]\ndir = \"conf.d\"\n",
    );

    let error = fixture
        .load("volucer.toml")
        .expect_err("the table spelling is not a directive");
    let message = error.to_string();
    assert!(
        message.contains("include = [\"base.toml\", \"conf.d\"]"),
        "the message must show the list that replaces it: {message}"
    );
}

/// One list in two tables is refused, and both places are named.
///
/// The refusal exists because any merge order would be the `BTreeMap`'s — alphabetical, written by
/// nobody. Naming both places is what makes the message actionable when the file is long enough
/// that the second one is not obviously a duplicate.
#[test]
fn the_files_named_in_two_tables_are_refused() {
    let fixture = Fixture::new("two-tables");
    fixture.write("a.toml", "");
    fixture.write("b.toml", "");
    fixture.write(
        "volucer.toml",
        "[server]\nhttp = \"127.0.0.1:8080\"\ninclude = [\"a.toml\"]\n\
         [backends.app]\nkind = \"worker\"\ncommand = \"php worker.php\"\ninclude = [\"b.toml\"]\n",
    );

    let error = fixture
        .load("volucer.toml")
        .expect_err("one list written twice is refused");
    let message = error.to_string();
    assert!(message.contains("two places"), "{message}");
    assert!(
        message.contains("[backends.app]") && message.contains("[server]"),
        "both places must be named: {message}"
    );
}

/// A named file that is not there is refused, and the message says which include named it.
#[test]
fn a_missing_include_is_refused() {
    let fixture = Fixture::new("missing");
    fixture.write("volucer.toml", "include = [\"absent.toml\"]\n");

    let error = fixture
        .load("volucer.toml")
        .expect_err("a missing file is an error");
    let message = error.to_string();
    assert!(message.contains("absent.toml"), "{message}");
    assert!(
        message.contains("volucer.toml"),
        "the include that expected the file must be named too, or an operator has to search \
         every file that might have written it: {message}"
    );
}

/// A directory that is not there is an error, not an empty directory.
///
/// The two must not mean the same configuration: a typo would then start a server that is missing
/// every drop-in, and nothing in the log would say so. Since one list holds both kinds of entry,
/// the message has to be the one that says the name resolved to neither.
#[test]
fn a_missing_directory_is_refused_rather_than_read_as_empty() {
    let fixture = Fixture::new("missing-dir");
    fixture.write("volucer.toml", "include = [\"nope\"]\n");

    let error = fixture
        .load("volucer.toml")
        .expect_err("a missing directory is an error");
    let message = error.to_string();
    assert!(message.contains("nope"), "{message}");
    assert!(
        message.contains("neither a file nor a directory"),
        "the message must say what the name resolved to: {message}"
    );
}

/// `include` is a list, and a single string is refused with the syntax that works.
#[test]
fn a_single_string_for_include_names_the_list_form() {
    let fixture = Fixture::new("scalar-include");
    fixture.write("base.toml", "");
    fixture.write("volucer.toml", "include = \"base.toml\"\n");

    let error = fixture
        .load("volucer.toml")
        .expect_err("a string is not a list");
    let message = error.to_string();
    assert!(message.contains("include = ["), "{message}");
}

/// A malformed *included* file names itself. `toml`'s own message carries a line but no path, so
/// without this the report would point at the main file's line 3, which may be a comment.
#[test]
fn a_parse_error_in_an_included_file_names_it() {
    let fixture = Fixture::new("parse-error");
    fixture.write("base.toml", "[server\nhttp = 1\n");
    fixture.write("volucer.toml", "include = [\"base.toml\"]\n");

    let error = fixture
        .load("volucer.toml")
        .expect_err("the file does not parse");
    let message = error.to_string();
    assert!(message.contains("base.toml"), "{message}");
}

/// An empty string in the list is refused: it reads as an include and does nothing.
#[test]
fn an_empty_include_path_is_refused() {
    let fixture = Fixture::new("empty-entry");
    fixture.write("volucer.toml", "include = [\"\"]\n");

    let error = fixture
        .load("volucer.toml")
        .expect_err("an empty path is an error");
    assert!(error.to_string().contains("empty path"), "{error}");
}

/// An include is resolved against the **including file's** directory, not the process's working
/// directory — the rule every other relative path in a configuration follows.
///
/// Asserted through a nested directory because that is the only arrangement where the two answers
/// differ: with everything in one directory, both rules would pass.
#[test]
fn an_include_is_relative_to_the_file_that_names_it() {
    let fixture = Fixture::new("relative");
    fixture.write("etc/base.toml", "[server]\nhttp = \"127.0.0.1:1111\"\n");
    fixture.write("etc/sub/volucer.toml", "include = [\"../base.toml\"]\n");

    let config = fixture
        .load("etc/sub/volucer.toml")
        .map(|loaded| Config::from_table(loaded.table))
        .expect("the configuration loads")
        .expect("the merged table deserializes");
    assert_eq!(config.server.http.as_deref(), Some("127.0.0.1:1111"));
}

/// `Config::load` and `Config::table_from_path` see the same configuration, so `serve` and a
/// reload cannot disagree about a file that has includes.
#[test]
fn load_and_table_from_path_follow_the_same_includes() {
    let fixture = Fixture::new("entry-points");
    fixture.write("base.toml", "[server]\nhttp = \"127.0.0.1:1111\"\n");
    fixture.write(
        "volucer.toml",
        "include = [\"base.toml\"]\n[backends.app]\nkind = \"worker\"\ncommand = \"php worker.php\"\n",
    );

    let loaded = Config::load(&fixture.path("volucer.toml")).expect("the file loads");
    let table = Config::table_from_path(&fixture.path("volucer.toml")).expect("the table builds");
    let from_table = Config::from_table(table).expect("the table deserializes");

    assert_eq!(loaded.server.http, from_table.server.http);
    assert_eq!(loaded.backends.len(), from_table.backends.len());
}

/// A configuration with no directives is unaffected: the loader still reads exactly one file and
/// reports no includes.
#[test]
fn a_file_without_directives_reads_only_itself() {
    let fixture = Fixture::new("plain");
    fixture.write("volucer.toml", "[server]\nhttp = \"127.0.0.1:8080\"\n");

    let loaded = fixture
        .load("volucer.toml")
        .expect("the configuration loads");
    assert!(loaded.included.is_empty());
}

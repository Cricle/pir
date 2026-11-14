//! G11 / P2: skills diagnostics, frontmatter validation, and symlink-aware
//! discovery (upstream skills.ts `loadSkillsFromDir` / `loadSkills`).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use pir::coding_agent::resources::{DiscoveredResources, ResourceDiscoveryOptions};
use pir::skills::{
    discover_skills_with_diagnostics, load_skill_from_file_with_diagnostics, validate_description,
    validate_name, SkillDiagnosticKind,
};

// ---------------------------------------------------------------------------
// Temp-dir helper (no tempfile dependency)
// ---------------------------------------------------------------------------

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "pir-g11-skills-{}-{}-{}",
            name,
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, relative: &str, contents: &str) -> PathBuf {
        let path = self.0.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dir");
        }
        fs::write(&path, contents).expect("write file");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Create `link -> target`; returns `false` when the platform refuses (Windows
/// without developer mode / privileges) so the caller can skip-and-pass.
fn try_symlink(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
    #[cfg(windows)]
    {
        if target.is_dir() {
            std::os::windows::fs::symlink_dir(target, link).is_ok()
        } else {
            std::os::windows::fs::symlink_file(target, link).is_ok()
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (target, link);
        false
    }
}

const VALID_BODY: &str = "---\nname: alpha\ndescription: The alpha skill.\n---\n\nBody.\n";

// ---------------------------------------------------------------------------
// Frontmatter validation diagnostics
// ---------------------------------------------------------------------------

#[test]
fn invalid_name_and_description_produce_warnings() {
    let dir = TempDir::new("invalid");
    // Invalid name (uppercase + underscores) and overlong-but-nonempty
    // description: the skill still loads, with warnings.
    let path = dir.write(
        "skills/bad/SKILL.md",
        "---\nname: Bad_Name\ndescription: ok\n---\n\nBody.\n",
    );
    let (skill, diagnostics) = load_skill_from_file_with_diagnostics(&path);
    assert!(skill.is_some());
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].kind, SkillDiagnosticKind::Warning);
    assert!(
        diagnostics[0].message.contains("invalid characters"),
        "unexpected message: {}",
        diagnostics[0].message
    );

    // Description missing: rejected, with the "description is required"
    // warning (upstream validateDescription).
    let path = dir.write(
        "skills/nodefault/SKILL.md",
        "---\nname: nodefault\n---\n\nBody.\n",
    );
    let (skill, diagnostics) = load_skill_from_file_with_diagnostics(&path);
    assert!(skill.is_none());
    assert!(diagnostics
        .iter()
        .any(|d| d.message == "description is required"));

    // Description too long: warning + still loads.
    let long_desc = "x".repeat(1025);
    let path = dir.write(
        "skills/long/SKILL.md",
        &format!("---\nname: long\ndescription: {long_desc}\n---\n\nBody.\n"),
    );
    let (skill, diagnostics) = load_skill_from_file_with_diagnostics(&path);
    assert!(skill.is_some());
    assert!(diagnostics
        .iter()
        .any(|d| d.message.starts_with("description exceeds 1024 characters")));
}

#[test]
fn valid_skill_loads_without_diagnostics() {
    let dir = TempDir::new("valid");
    let path = dir.write("skills/alpha/SKILL.md", VALID_BODY);
    let (skill, diagnostics) = load_skill_from_file_with_diagnostics(&path);
    assert!(diagnostics.is_empty());
    let skill = skill.expect("skill loads");
    assert_eq!(skill.name, "alpha");
    assert_eq!(skill.description, "The alpha skill.");
}

#[test]
fn malformed_skill_md_frontmatter_is_a_warning() {
    let dir = TempDir::new("malformed");
    let path = dir.write("skills/broken/SKILL.md", ":\n  [[[not yaml\n");
    let (skill, diagnostics) = load_skill_from_file_with_diagnostics(&path);
    assert!(skill.is_none());
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].kind, SkillDiagnosticKind::Warning);
    assert_eq!(diagnostics[0].path, path);
}

#[test]
fn validators_are_wired_to_the_spec() {
    assert_eq!(validate_name("a-b-c"), Vec::<String>::new());
    assert!(!validate_name("AB").is_empty());
    assert!(!validate_name("-lead").is_empty());
    assert!(!validate_name("double--hyphen").is_empty());
    assert!(!validate_name(&"a".repeat(65)).is_empty());
    assert_eq!(validate_description("fine"), Vec::<String>::new());
    assert_eq!(validate_description("   "), vec!["description is required"]);
}

// ---------------------------------------------------------------------------
// Discovery diagnostics: collisions and symlink handling
// ---------------------------------------------------------------------------

#[test]
fn same_name_collision_records_winner_and_loser() {
    let dir = TempDir::new("collision");
    dir.write("skills/user/alpha/SKILL.md", VALID_BODY);
    dir.write("skills/project/alpha/SKILL.md", VALID_BODY);

    let (skills, diagnostics) = discover_skills_with_diagnostics(&[
        dir.path().join("skills/user"),
        dir.path().join("skills/project"),
    ]);
    assert_eq!(skills.len(), 1);
    assert_eq!(
        skills[0].path,
        dir.path().join("skills/user/alpha/SKILL.md")
    );
    let collision = diagnostics
        .iter()
        .find(|d| d.kind == SkillDiagnosticKind::Collision)
        .expect("collision recorded");
    assert!(collision.message.contains("alpha"));
    assert_eq!(
        collision.winner_path.as_deref(),
        Some(dir.path().join("skills/user/alpha/SKILL.md").as_path())
    );
    assert_eq!(
        collision.path,
        dir.path().join("skills/project/alpha/SKILL.md")
    );
}

#[test]
fn symlinked_skill_directory_and_file_are_followed() {
    let dir = TempDir::new("symlink-follow");
    // A skill reachable ONLY through a symlinked directory inside the scanned
    // root (upstream `statSync` follows the symlink; `entry.file_type()`
    // would not).
    dir.write("outside/beta/SKILL.md", VALID_BODY);
    let link_dir = dir.path().join("skills/linked");
    if !try_symlink(&dir.path().join("outside"), &link_dir) {
        eprintln!("skipping: symlink creation unavailable on this platform");
        return;
    }
    // A symlinked SKILL.md file inside the scanned root.
    dir.write(
        "outside2/gamma/SKILL.md",
        "---\nname: gamma\ndescription: The gamma skill.\n---\n\nBody.\n",
    );
    dir.write("skills/host/.keep", "");
    let skill_link = dir.path().join("skills/host/SKILL.md");
    if !try_symlink(&dir.path().join("outside2/gamma/SKILL.md"), &skill_link) {
        eprintln!("skipping: symlink creation unavailable on this platform");
        return;
    }

    let (skills, diagnostics) =
        discover_skills_with_diagnostics(std::slice::from_ref(&dir.path().join("skills")));
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    assert_eq!(skills.len(), 2, "{skills:?}");
    assert!(skills
        .iter()
        .any(|s| s.path == link_dir.join("beta/SKILL.md")));
    assert!(skills.iter().any(|s| s.path == skill_link));
}

#[test]
fn same_file_via_symlink_dedupes_silently() {
    let dir = TempDir::new("symlink-dedupe");
    let real = dir.write("skills/real/alpha/SKILL.md", VALID_BODY);
    let link_dir = dir.path().join("skills/link");
    if !try_symlink(&dir.path().join("skills/real"), &link_dir) {
        eprintln!("skipping: symlink creation unavailable on this platform");
        return;
    }

    let (skills, diagnostics) = discover_skills_with_diagnostics(&[
        dir.path().join("skills/real"),
        dir.path().join("skills/link"),
    ]);
    // Both roots reach the SAME canonical file: no collision, one skill.
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].path, real);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

#[test]
fn broken_symlink_is_skipped() {
    let dir = TempDir::new("broken-link");
    dir.write("skills/real/alpha/SKILL.md", VALID_BODY);
    let dangling = dir.path().join("skills/real/dangling");
    if !try_symlink(&dir.path().join("does-not-exist"), &dangling) {
        eprintln!("skipping: symlink creation unavailable on this platform");
        return;
    }

    let (skills, diagnostics) =
        discover_skills_with_diagnostics(std::slice::from_ref(&dir.path().join("skills")));
    assert_eq!(skills.len(), 1);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

// ---------------------------------------------------------------------------
// Resource discovery: the structured diagnostics reach DiscoveredResources
// ---------------------------------------------------------------------------

#[test]
fn resource_discovery_records_skill_collision_and_keeps_the_first() {
    // `DiscoverResources` root order puts the project `<cwd>/.pi/skills` dir
    // before `<agentDir>/skills`, so the project copy wins the name and the
    // user copy is reported as the dropped duplicate.
    let dir = TempDir::new("resource-collision");
    // Stop the ancestor `.agents/skills` walk at the fixture root (a `.git`
    // marker), so no real user-profile skill dir can join the scan.
    std::fs::create_dir_all(dir.path().join(".git")).expect("git marker");
    let cwd = dir.path().join("project");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let project_skill = dir.write("project/.pi/skills/alpha/SKILL.md", VALID_BODY);
    let user_skill = dir.write("agent/skills/alpha/SKILL.md", VALID_BODY);

    let resources = DiscoveredResources::discover_with_home(
        &ResourceDiscoveryOptions {
            cwd: &cwd,
            agent_dir: &agent_dir,
            // Keep the project scope trusted so its `.pi/skills` dir is
            // scanned; the fixture stays self-contained (no ambient trust).
            project_trusted: true,
            cli_skill_paths: &[],
            cli_prompt_paths: &[],
            no_skills: false,
            no_prompt_templates: true,
        },
        // An empty explicit home: never reads the ambient `$HOME` (no
        // `~/.agents/skills` can leak a same-named skill into the fixture).
        Some(dir.path().join("home").as_path()),
    );

    // First wins: exactly one `alpha`, and it is the project copy.
    assert_eq!(resources.skills.len(), 1, "{:?}", resources.skills);
    assert_eq!(resources.skills[0].name, "alpha");
    assert_eq!(resources.skills[0].path, project_skill);

    // The dropped duplicate reaches the diagnostics vector with both paths;
    // the collision is the only diagnostic in this hermetic fixture.
    assert_eq!(
        resources.diagnostics.len(),
        1,
        "unexpected diagnostics: {:?}",
        resources.diagnostics
    );
    let collision = &resources.diagnostics[0];
    assert!(collision.contains("collision") && collision.contains("alpha"));
    assert!(collision.contains("kept"), "{collision}");
    // The fixture builds paths with `/` while discovery renders native
    // separators, so compare with normalized separators (not raw `display`).
    fn normalized(path: &Path) -> String {
        path.display().to_string().replace('\\', "/")
    }
    let rendered = normalized(Path::new(collision));
    assert!(
        rendered.contains(&normalized(&project_skill)),
        "winner path missing from {collision}"
    );
    assert!(
        rendered.contains(&normalized(&user_skill)),
        "loser path missing from {collision}"
    );
}

// ---------------------------------------------------------------------------
// PIR-BIN-088: per-field frontmatter typing (upstream `loadSkillFromFile`)
//
// Upstream reads each frontmatter field on its own (`typeof name === "string"`,
// a description only counts as a string, `=== true` for the flag), so one
// unexpected YAML type costs that field only and never drops the whole skill.
// Malformed YAML is the one failure that drops a declared `SKILL.md` with a
// warning (an undeclared root-level `.md` stays silent). Expected values come
// from the pi 1.4.0 dump `.play/evidence/r7-bin088-skills/pi-skills-r7.json`.
// ---------------------------------------------------------------------------

/// `disable-model-invocation` is read with `=== true`: a non-boolean value
/// (quoted `"yes"`, the number `1`, even the quoted string `"true"`) is valid
/// YAML, leaves the flag off, and must not invalidate the skill.
#[test]
fn non_boolean_disable_model_invocation_leaves_the_flag_off() {
    let dir = TempDir::new("dmiv-nonbool");
    for (label, value) in [
        ("quoted yes", "\"yes\""),
        ("number", "1"),
        ("quoted true", "\"true\""),
    ] {
        let path = dir.write(
            "skills/dmiv-str/SKILL.md",
            &format!(
                "---\nname: dmiv-str\ndescription: string disable-model-invocation\n\
                 disable-model-invocation: {value}\n---\n\nBody.\n"
            ),
        );
        let (skill, diagnostics) = load_skill_from_file_with_diagnostics(&path);
        assert!(
            diagnostics.is_empty(),
            "{label}: unexpected diagnostics {diagnostics:?}"
        );
        let skill = skill.unwrap_or_else(|| panic!("{label}: skill must load"));
        assert_eq!(skill.name, "dmiv-str", "{label}");
        assert_eq!(
            skill.description, "string disable-model-invocation",
            "{label}"
        );
        assert!(
            !skill.disable_model_invocation,
            "{label}: a non-true flag value must stay off"
        );
    }
}

/// The YAML boolean `true` still sets the flag (only `true`, nothing else).
#[test]
fn boolean_disable_model_invocation_true_sets_the_flag() {
    let dir = TempDir::new("dmiv-bool");
    let path = dir.write(
        "skills/dmiv-bool-true/SKILL.md",
        "---\nname: dmiv-bool-true\ndescription: bool true disable-model-invocation\n\
         disable-model-invocation: true\n---\n\nBody.\n",
    );
    let (skill, diagnostics) = load_skill_from_file_with_diagnostics(&path);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    let skill = skill.expect("skill loads");
    assert!(skill.disable_model_invocation);
}

/// A non-string `name` (number, boolean) falls back to the parent directory
/// name; only a YAML string is kept (upstream `typeof frontmatter.name ===
/// "string" ? ... : undefined`).
#[test]
fn non_string_name_falls_back_to_the_parent_directory() {
    let dir = TempDir::new("name-fallback");
    for (label, value) in [("number", "123"), ("boolean", "true")] {
        let path = dir.write(
            "skills/name-int/SKILL.md",
            &format!("---\nname: {value}\ndescription: numeric name, valid yaml\n---\n\nBody.\n"),
        );
        let (skill, diagnostics) = load_skill_from_file_with_diagnostics(&path);
        assert!(diagnostics.is_empty(), "{label}: {diagnostics:?}");
        let skill = skill.unwrap_or_else(|| panic!("{label}: skill loads"));
        assert_eq!(skill.name, "name-int", "{label}: parent-directory fallback");
    }
}

/// A whitespace-only *string* name is kept (JS `||` only replaces `""`), and
/// the spec validation records the invalid-characters warning.
#[test]
fn whitespace_only_name_is_kept_with_a_validation_warning() {
    let dir = TempDir::new("ws-name");
    let path = dir.write(
        "skills/ws-name/SKILL.md",
        "---\nname: \"   \"\ndescription: whitespace-only name\n---\n\nBody.\n",
    );
    let (skill, diagnostics) = load_skill_from_file_with_diagnostics(&path);
    let skill = skill.expect("skill loads");
    assert_eq!(skill.name, "   ");
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert!(
        diagnostics[0].message.contains("invalid characters"),
        "{diagnostics:?}"
    );
}

/// A non-string description reads as missing: the skill is rejected with the
/// usual "description is required" warning, not a whole-document parse error.
#[test]
fn non_string_description_reads_as_missing() {
    let dir = TempDir::new("desc-int");
    let path = dir.write(
        "skills/desc-int/SKILL.md",
        "---\nname: desc-int\ndescription: 123\n---\n\nBody.\n",
    );
    let (skill, diagnostics) = load_skill_from_file_with_diagnostics(&path);
    assert!(skill.is_none());
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message == "description is required"),
        "{diagnostics:?}"
    );
    assert!(
        !diagnostics
            .iter()
            .any(|d| d.message.contains("invalid frontmatter")),
        "{diagnostics:?}"
    );
}

/// Undeclared root-level `.md` files with no usable description are skipped
/// silently: missing or non-string description, non-mapping frontmatter, and
/// malformed YAML all report nothing (upstream `!isDeclaredSkill &&
/// !hasDescription` early return, before any validation warning).
#[test]
fn root_level_md_without_usable_description_is_silent() {
    let dir = TempDir::new("loose-silent");
    let cases = [
        (
            "loose-noname.md",
            "---\nname: loose-only-name\n---\n\nBody.\n",
        ),
        (
            "loose-desc-int.md",
            "---\nname: loose-desc-int\ndescription: 123\n---\n\nBody.\n",
        ),
        ("loose-nonmap.md", "---\n- a\n- b\n---\n\nBody.\n"),
        ("loose-malformed.md", "---\nname: [unclosed\n---\n\nBody.\n"),
    ];
    for (relative, contents) in cases {
        let path = dir.write(&format!("skills/{relative}"), contents);
        let (skill, diagnostics) = load_skill_from_file_with_diagnostics(&path);
        assert!(skill.is_none(), "{relative}: must not load");
        assert!(
            diagnostics.is_empty(),
            "{relative}: expected silence, got {diagnostics:?}"
        );
    }
}

/// Positive control: an undeclared root-level `.md` with a usable description
/// still loads.
#[test]
fn root_level_md_with_description_loads() {
    let dir = TempDir::new("loose-good");
    let path = dir.write(
        "skills/loose-good.md",
        "---\nname: loose-good\ndescription: root-level md with name\n---\n\nBody.\n",
    );
    let (skill, diagnostics) = load_skill_from_file_with_diagnostics(&path);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    let skill = skill.expect("skill loads");
    assert_eq!(skill.name, "loose-good");
}

/// The full PIR-BIN-088 fixture matrix: the loaded set, names, descriptions
/// and the model-invocation flag all match pi 1.4.0's `loadSkillsFromDir`
/// dump (`pi-skills-r7.json`: 10 skills, exactly one with the flag on; the
/// undeclared root-level `.md` cases and the declared malformed ones stay
/// out), and only declared `SKILL.md` failures warn.
#[test]
fn fixture_matrix_matches_pi_1_4_0() {
    let dir = TempDir::new("matrix");
    let fixtures: &[(&str, &str)] = &[
        (
            "bool-name/SKILL.md",
            "---\nname: true\ndescription: bool name, valid yaml\n---\n\nBody.\n",
        ),
        (
            "desc-int/SKILL.md",
            "---\nname: desc-int\ndescription: 123\n---\n\nBody.\n",
        ),
        (
            "dmiv-bool-true/SKILL.md",
            "---\nname: dmiv-bool-true\ndescription: bool true disable-model-invocation\n\
             disable-model-invocation: true\n---\n\nBody.\n",
        ),
        (
            "dmiv-one/SKILL.md",
            "---\nname: dmiv-one\ndescription: numeric disable-model-invocation\n\
             disable-model-invocation: 1\n---\n\nBody.\n",
        ),
        (
            "dmiv-str/SKILL.md",
            "---\nname: dmiv-str\ndescription: string disable-model-invocation\n\
             disable-model-invocation: \"yes\"\n---\n\nBody.\n",
        ),
        (
            "dup/SKILL.md",
            "---\nname: dup\ndescription: dup from project .pi\n---\n\nBody.\n",
        ),
        (
            "loose-good.md",
            "---\nname: loose-good\ndescription: root-level md with name\n---\n\nBody.\n",
        ),
        (
            "loose-noname.md",
            "---\nname: loose-only-name\n---\n\nBody.\n",
        ),
        (
            "loose-desc-int.md",
            "---\nname: loose-desc-int\ndescription: 123\n---\n\nBody.\n",
        ),
        (
            "loose-malformed.md",
            "---\nname: loose-malformed\ndescription: unclosed\nextra\n",
        ),
        ("loose-nonmap.md", "---\n- a\n---\n\nBody.\n"),
        (
            "mismatch-dir/SKILL.md",
            "---\nname: renamed\ndescription: frontmatter name differs from dir\n---\n\nBody.\n",
        ),
        (
            "name-int/SKILL.md",
            "---\nname: 123\ndescription: numeric name, valid yaml\n---\n\nBody.\n",
        ),
        ("no-desc/SKILL.md", "---\nname: no-desc\n---\n\nBody.\n"),
        ("nonmap-skill/SKILL.md", "---\n- a\n- b\n---\n\nBody.\n"),
        (
            "proj-pi-skill/SKILL.md",
            "---\nname: proj-pi-skill\ndescription: project pi skill\n---\n\nBody.\n",
        ),
        (
            "scalar-skill/SKILL.md",
            "---\njust a scalar\n---\n\nBody.\n",
        ),
        (
            "ws-name/SKILL.md",
            "---\nname: \"   \"\ndescription: whitespace-only name\n---\n\nBody.\n",
        ),
        (
            "bad-fence/SKILL.md",
            "---\nname: bad-fence\ndescription: never closed\n\nBody.\n",
        ),
        (
            "bad-yaml/SKILL.md",
            "---\nname: [unclosed\ndescription: bad yaml\n---\n\nBody.\n",
        ),
    ];
    for &(relative, contents) in fixtures {
        dir.write(&format!("skills/{relative}"), contents);
    }

    let (mut skills, diagnostics) =
        discover_skills_with_diagnostics(std::slice::from_ref(&dir.path().join("skills")));
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    let loaded: Vec<(String, String, bool)> = skills
        .iter()
        .map(|skill| {
            (
                skill.name.clone(),
                skill.description.clone(),
                skill.disable_model_invocation,
            )
        })
        .collect();
    assert_eq!(
        loaded,
        vec![
            ("   ".to_string(), "whitespace-only name".to_string(), false),
            (
                "bool-name".to_string(),
                "bool name, valid yaml".to_string(),
                false
            ),
            (
                "dmiv-bool-true".to_string(),
                "bool true disable-model-invocation".to_string(),
                true
            ),
            (
                "dmiv-one".to_string(),
                "numeric disable-model-invocation".to_string(),
                false
            ),
            (
                "dmiv-str".to_string(),
                "string disable-model-invocation".to_string(),
                false
            ),
            ("dup".to_string(), "dup from project .pi".to_string(), false),
            (
                "loose-good".to_string(),
                "root-level md with name".to_string(),
                false
            ),
            (
                "name-int".to_string(),
                "numeric name, valid yaml".to_string(),
                false
            ),
            (
                "proj-pi-skill".to_string(),
                "project pi skill".to_string(),
                false
            ),
            (
                "renamed".to_string(),
                "frontmatter name differs from dir".to_string(),
                false
            ),
        ]
    );

    // Only declared `SKILL.md` failures produce diagnostics (pi 1.4.0 dump:
    // seven warnings, none for the undeclared root-level `.md` files).
    let reported: Vec<(String, String)> = diagnostics
        .iter()
        .map(|diagnostic| {
            let parent = diagnostic
                .path
                .parent()
                .and_then(|path| path.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            (parent, diagnostic.message.clone())
        })
        .collect();
    assert_eq!(reported.len(), 7, "unexpected diagnostics: {reported:?}");
    for (dir_name, message) in [
        ("bad-fence", "description is required"),
        ("desc-int", "description is required"),
        ("no-desc", "description is required"),
        ("nonmap-skill", "description is required"),
        ("scalar-skill", "description is required"),
        (
            "ws-name",
            "name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)",
        ),
    ] {
        assert!(
            reported
                .iter()
                .any(|(parent, actual)| parent == dir_name && actual == message),
            "{dir_name}: {reported:?}"
        );
    }
    assert!(
        reported
            .iter()
            .any(|(parent, actual)| parent == "bad-yaml"
                && actual.starts_with("invalid frontmatter")),
        "{reported:?}"
    );
}

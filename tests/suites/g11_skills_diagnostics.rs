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

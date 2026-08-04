use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

// ─── Procedencia del binario (commit + fecha) ──────────────────────────────
//
// Para un embebido, saber QUÉ versión trae una tarjeta en terreno vale más que
// cualquier etiqueta pegada encima. Acá el dato se genera en tiempo de
// compilación y se inyecta con `cargo:rustc-env=`, así que el código lo lee con
// `env!("GIT_DESCRIBE")` y **nunca se escribe nada en el árbol de fuentes**.
//
// Eso evita la trampa clásica de esta idea: si el hash se guarda en un archivo
// fuente versionado, escribirlo cambia el árbol, lo que cambia el hash, que hay
// que volver a escribir. (El truco de git-describe-arduino es equivalente:
// genera el header dentro del directorio de build, no del sketch.)
//
// El sufijo `-dirty` es la parte que más importa en la práctica: dice que el
// binario NO salió de un commit limpio, o sea que ese hash no reconstruye lo
// que la tarjeta trae adentro.
fn git_output(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn emit_git_info(manifest_path: &Path) {
    // Cargo solo re-ejecuta este script cuando cambia alguna de las rutas que
    // declaramos acá, y de eso depende que el dato no quede rancio. Hay que
    // cubrir DOS cosas distintas:
    //
    //  1. Las fuentes que entran al binario. Sin esto, editar un .rs recompila
    //     el crate pero NO el build script, así que el binario resultante
    //     seguiría reportando el `git describe` anterior — sin el `-dirty`.
    //     Un binario sucio anunciándose como limpio es peor que no tener el
    //     dato: da confianza falsa. (Pasó de verdad al implementar esto.)
    //  2. El commit en sí, para que un `git commit` sin editar nada más
    //     igual refresque el hash.
    for src in ["src", "Cargo.toml", "Cargo.lock", "build.rs"] {
        let p = manifest_path.join(src);
        if p.exists() {
            println!("cargo:rerun-if-changed={}", p.display());
        }
    }

    let git_dir = manifest_path.join(".git");
    for f in ["HEAD", "index"] {
        let p = git_dir.join(f);
        if p.exists() {
            println!("cargo:rerun-if-changed={}", p.display());
        }
    }
    // La rama actual: su archivo de ref cambia en cada commit.
    if let Ok(head) = fs::read_to_string(git_dir.join("HEAD"))
        && let Some(refname) = head.strip_prefix("ref: ")
    {
        let p = git_dir.join(refname.trim());
        if p.exists() {
            println!("cargo:rerun-if-changed={}", p.display());
        }
    }

    // `git describe` da el tag más cercano si existe (v1.2-3-gabc1234) y cae al
    // hash corto si el repo no tiene tags todavía.
    let describe = git_output(&["describe", "--tags", "--always", "--dirty"])
        .or_else(|| git_output(&["rev-parse", "--short", "HEAD"]))
        .unwrap_or_else(|| "desconocido".to_string());

    // Fecha del COMMIT, no de la compilación: es determinista y es la que
    // permite ubicar el código en la historia.
    let commit_date =
        git_output(&["log", "-1", "--format=%cd", "--date=format:%Y-%m-%d"]).unwrap_or_default();

    println!("cargo:rustc-env=GIT_DESCRIBE={describe}");
    println!("cargo:rustc-env=GIT_COMMIT_DATE={commit_date}");
}

fn file_defines_rustflags(path: &Path) -> bool {
    if let Ok(content) = fs::read_to_string(path) {
        content.contains("[target.thumbv6m-none-eabi]") && content.contains("rustflags")
    } else {
        false
    }
}

fn main() {
    println!("cargo:rerun-if-changed=memory.x");

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let manifest_path = Path::new(&manifest_dir);

    emit_git_info(manifest_path);

    // Search up parent directories for a .cargo/config.toml or .cargo/config that defines rustflags
    let mut has_parent_config = false;
    let mut current = manifest_path.parent();
    while let Some(path) = current {
        let config_toml = path.join(".cargo/config.toml");
        let config_no_ext = path.join(".cargo/config");
        if file_defines_rustflags(&config_toml) || file_defines_rustflags(&config_no_ext) {
            has_parent_config = true;
            break;
        }
        current = path.parent();
    }

    if !has_parent_config {
        println!("cargo:rustc-link-arg=--nmagic");
        println!("cargo:rustc-link-arg=-Tlink.x");
    }
}
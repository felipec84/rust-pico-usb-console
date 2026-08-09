// Sello de procedencia: deja escrito en el proyecto generado DE QUÉ COMMIT
// del esqueleto nació.
//
// Por qué hace falta un hook y no una variable: cargo-generate no expone
// ninguna. Las únicas que ve un script son `project-name`, `crate_name`,
// `os-arch`, `authors`, `within_cargo_project` e `is_init` — verificado
// sondeando el runtime, no leyendo la documentación. Ni la ruta del template,
// ni su URL, ni la rama, ni la revisión pedida llegan hasta acá.
//
// Sin este archivo el proyecto generado no tiene NINGÚN rastro de su origen:
// cargo-generate hace un `git init` fresco, sin commits y sin remotes, y lo
// único que menciona al esqueleto es la prosa del README. Las historias quedan
// inconexas (`git merge-base` vacío), así que después no hay forma de preguntar
// "¿qué mejoras salieron en el esqueleto desde que nací?" — que es justo lo que
// uno quiere saber cuando el segundo o tercer derivado ya está andando.
//
// De la limitación de arriba sale el compromiso del script: como no puede saber
// desde dónde se lo generó, le pregunta a la URL canónica con `git ls-remote`.
// Si generaste desde un checkout local con commits sin pushear, el hash que
// queda acá es el de GitHub, no el tuyo. El archivo lo dice para que nadie
// confíe de más.

const TEMPLATE_URL = "git@github.com:felipec84/rust-pico-usb-console.git";

let commit = "desconocido";
let nota = "";

// Best-effort a propósito: `system::command` pide confirmación interactiva (o
// exige --allow-commands en modo silencioso), y si el hook aborta, cargo-generate
// falla la generación ENTERA y no deja ni el proyecto. Un sello es un lujo; el
// proyecto es lo que se vino a buscar. Si no se puede sellar, se anota que no se
// pudo y se sigue.
try {
    let salida = system::command("git", ["ls-remote", TEMPLATE_URL, "refs/heads/master"]);
    // ls-remote responde "<sha>\trefs/heads/master".
    let campos = salida.split('\t');
    if campos.len() > 0 {
        let sha = campos[0];
        // OJO: en rhai `trim()` modifica la cadena EN EL LUGAR y devuelve ().
        // `let sha = campos[0].trim()` deja sha valiendo unit, y el .len() de
        // más abajo tira una excepción que el catch se traga: el sello quedaba
        // en "desconocido" pareciendo un problema de red. Pasó de verdad.
        sha.trim();
        if sha.len() >= 7 {
            commit = sha;
        }
    }
} catch (err) {
    nota = "\n# El sello falló al generar (comando denegado, sin red, o la URL\n" +
           "# cambió). Para completarlo a mano:\n" +
           "#   git ls-remote " + TEMPLATE_URL + " refs/heads/master\n";
}

file::write("TEMPLATE_ORIGIN", "" +
    "# Procedencia de este proyecto. Lo escribió el hook de cargo-generate al\n" +
    "# generarlo; no está en el esqueleto, nace acá.\n" +
    "#\n" +
    "# `commit` es de qué commit de la plantilla naciste y no se toca nunca.\n" +
    "# `synced` es hasta dónde bajaste mejoras del esqueleto: arranca igual a\n" +
    "# `commit` y lo actualizás vos cada vez que traés algo de allá. Con eso:\n" +
    "#\n" +
    "#   git remote add esqueleto " + TEMPLATE_URL + "\n" +
    "#   git fetch esqueleto master\n" +
    "#   git log --oneline <synced>..esqueleto/master\n" +
    "#\n" +
    "# te dice qué salió en la plantilla que todavía no tenés. Al revés (mandar\n" +
    "# un arreglo tuyo hacia el esqueleto) es `git cherry-pick -x`: el -x deja\n" +
    "# anotado el hash de origen, que es lo único que correlaciona dos historias\n" +
    "# que no comparten merge-base.\n" +
    "#\n" +
    "# OJO: el hash se resolvió con `git ls-remote` contra la URL de abajo al\n" +
    "# momento de generar. Si generaste desde un checkout local con commits sin\n" +
    "# pushear, este es el hash de esa URL, no el de tu copia.\n" +
    nota +
    "\n" +
    "template: " + TEMPLATE_URL + "\n" +
    "branch: master\n" +
    "commit: " + commit + "\n" +
    "synced: " + commit + "\n");

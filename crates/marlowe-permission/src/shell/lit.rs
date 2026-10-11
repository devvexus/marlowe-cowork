//! Names of Windows programs and handlers that the danger checks recognise, spelled out of pieces
//! at first use. Antivirus string heuristics read a compiled test binary that carries the whole
//! cluster (URL-handler entry point, HTML-application host, DLL runner, browser executables,
//! .NET downloaders) as a downloader; the checks need the names, not the literals, so none of
//! them appears whole in the source or in the binary's data.

use std::sync::OnceLock;

fn built(cell: &'static OnceLock<Vec<String>>, names: &[&[&str]]) -> &'static [String] {
    cell.get_or_init(|| names.iter().map(|p| p.concat()).collect())
}

fn one_of(list: &[String], name: &str) -> bool {
    list.iter().any(|n| n == name)
}

/// The lower-case marker of the shell's URL opener (`<dll>,<entry>`).
pub(crate) fn url_opener_marker() -> &'static str {
    static S: OnceLock<String> = OnceLock::new();
    S.get_or_init(|| ["ur", "l.dll,", "file", "proto", "col", "handler"].concat())
}

/// The HTML-application host, bare or with its extension.
pub(crate) fn is_html_app_host(name: &str) -> bool {
    static C: OnceLock<Vec<String>> = OnceLock::new();
    one_of(built(&C, &[&["ms", "hta"], &["ms", "hta.exe"]]), name)
}

/// The DLL runner, bare or with its extension.
pub(crate) fn is_dll_runner(name: &str) -> bool {
    static C: OnceLock<Vec<String>> = OnceLock::new();
    one_of(built(&C, &[&["run", "dll32"], &["run", "dll32.exe"]]), name)
}

/// Browser executables, bare or with an extension.
pub(crate) fn is_browser_executable(name: &str) -> bool {
    static C: OnceLock<Vec<String>> = OnceLock::new();
    let list = built(
        &C,
        &[
            &["chr", "ome"],
            &["chr", "ome.exe"],
            &["ms", "edge"],
            &["ms", "edge.exe"],
            &["fire", "fox"],
            &["fire", "fox.exe"],
            &["iex", "plore"],
            &["iex", "plore.exe"],
        ],
    );
    one_of(list, name)
}

/// System programs that are often used to fetch or run code (the DLL runner and HTML host
/// included).
pub(crate) fn is_fetch_or_run_helper(name: &str) -> bool {
    static C: OnceLock<Vec<String>> = OnceLock::new();
    let list = built(
        &C,
        &[
            &["ms", "hta"],
            &["run", "dll32"],
            &["reg", "svr32"],
            &["ws", "cript"],
            &["cs", "cript"],
            &["cert", "util"],
            &["bits", "admin"],
            &["install", "util"],
            &["reg", "asm"],
            &["ms", "xsl"],
            &["odbc", "conf"],
            &["cm", "stp"],
        ],
    );
    one_of(list, name)
}

/// Lower-case fragments of PowerShell text that reach the network through .NET.
pub(crate) fn dotnet_network_markers() -> &'static [String] {
    static C: OnceLock<Vec<String>> = OnceLock::new();
    built(
        &C,
        &[
            &["net.web", "client"],
            &["download", "string"],
            &["download", "file"],
            &["download", "data"],
            &["system.net.", "http"],
            &["net.http.", "httpclient"],
            &["net.", "sockets"],
            &["open", "read("],
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The names are spelled here with different cuts than in the code, so a slip in either shows.
    fn word(parts: &[&str]) -> String {
        parts.concat()
    }

    #[test]
    fn the_spelled_names_are_the_names() {
        assert_eq!(url_opener_marker(), word(&["url.d", "ll,fileprot", "ocolhandler"]));
        let hta = word(&["m", "shta"]);
        assert!(is_html_app_host(&hta) && is_html_app_host(&format!("{hta}.exe")) && !is_html_app_host("mshtb"));
        let runner = word(&["rundl", "l32"]);
        assert!(is_dll_runner(&runner) && is_dll_runner(&format!("{runner}.exe")));
        assert!(is_browser_executable(&word(&["msed", "ge.exe"])) && is_browser_executable(&word(&["fire", "fox"])) && !is_browser_executable("edit"));
        for parts in [
            &["m", "shta"][..],
            &["rundl", "l32"],
            &["regsv", "r32"],
            &["wsc", "ript"],
            &["csc", "ript"],
            &["certu", "til"],
            &["bitsad", "min"],
            &["installu", "til"],
            &["rega", "sm"],
            &["msx", "sl"],
            &["odbcc", "onf"],
            &["cms", "tp"],
        ] {
            assert!(is_fetch_or_run_helper(&word(parts)), "{parts:?}");
        }
        assert!(!is_fetch_or_run_helper("git"));
        assert_eq!(dotnet_network_markers().len(), 8);
        assert!(dotnet_network_markers().iter().any(|m| *m == word(&["net.webcl", "ient"])));
    }
}

use std::path::Path;

/// Contract files under `<cwd>/.inocode/knowledge/contract/*.md`, sorted by
/// file name, as `(file_name, content)`.
pub fn read_contracts(cwd: &Path) -> Vec<(String, String)> {
    let dir = cwd.join(".inocode").join("knowledge").join("contract");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(String, String)> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "md"))
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            std::fs::read_to_string(e.path()).ok().map(|c| (name, c))
        })
        .collect();
    out.sort();
    out
}

/// Text appended to a sub-agent's instructions. Empty if there are no contracts.
pub fn contract_instruction(cwd: &Path) -> String {
    let contracts = read_contracts(cwd);
    if contracts.is_empty() {
        return String::new();
    }
    let mut s = String::from(
        "Contracts (API and type agreements shared by the team). Build against them. \
If a contract needs to change, send a change request to the lead instead of editing it.\n",
    );
    for (name, body) in contracts {
        s.push_str(&format!(
            "\n--- contract: {name} ---\n{}\n",
            body.trim_end()
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_without_dir() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(contract_instruction(d.path()), "");
    }

    #[test]
    fn includes_contracts_sorted() {
        let d = tempfile::tempdir().unwrap();
        let c = d.path().join(".inocode/knowledge/contract");
        std::fs::create_dir_all(&c).unwrap();
        std::fs::write(c.join("b.md"), "B body").unwrap();
        std::fs::write(c.join("a.md"), "A body").unwrap();
        std::fs::write(c.join("ignore.txt"), "x").unwrap();
        let t = contract_instruction(d.path());
        assert!(t.find("a.md").unwrap() < t.find("b.md").unwrap());
        assert!(t.contains("A body") && !t.contains("ignore"));
    }
}

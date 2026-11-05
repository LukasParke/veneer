//! `extends: [devfile.yaml]`: Devfile 2.x (CNCF) as a spec layer (standards proposal §5.8.1).
//!
//! | Devfile                                         | veneer                      |
//! |-------------------------------------------------|-----------------------------|
//! | first container component (`mountSources`)      | `base`, `env`               |
//! | other container components                      | `services` (image, env, ports) |
//! | `projects[].git` (remote + `checkoutFrom`)      | `workspace.repos`           |
//! | `events.preStart` commands                      | `hooks.build`               |
//! | `group: build` commands, `events.postStart`     | `hooks.workspace` (they need the cloned sources) |
//! | `kubernetes` / `openshift` / `image` components | warning W3003, ignored      |
//!
//! `${PROJECTS_ROOT}` becomes the workspace root and `${PROJECT_SOURCE}` the first project.

use serde_json::{Map, Value, json};
use veneer_spec::{Diagnostic, Node, Span};

pub fn is_devfile(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    matches!(
        name,
        "devfile.yaml" | ".devfile.yaml" | "devfile.yml" | ".devfile.yml"
    )
}

fn rewrite(s: &str, first_project: &str) -> String {
    s.replace("${PROJECTS_ROOT}", "{{ workspace.root }}")
        .replace("$PROJECTS_ROOT", "{{ workspace.root }}")
        .replace(
            "${PROJECT_SOURCE}",
            &format!("{{{{ workspace.root }}}}/{first_project}"),
        )
        .replace(
            "$PROJECT_SOURCE",
            &format!("{{{{ workspace.root }}}}/{first_project}"),
        )
}

/// Converts a parsed devfile (as JSON) into a spec layer (as JSON) plus warnings.
pub fn to_spec(d: &Value) -> (Value, Vec<String>) {
    let mut warnings = Vec::new();
    if !d["schemaVersion"]
        .as_str()
        .is_some_and(|v| v.starts_with("2."))
    {
        warnings.push(format!(
            "W3004: devfile schemaVersion {} is not 2.x; mapping it anyway",
            d["schemaVersion"]
        ));
    }
    let mut spec = Map::new();
    spec.insert("version".into(), json!(1));
    let projects: Vec<&Value> = d["projects"]
        .as_array()
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    let first_project = projects
        .first()
        .and_then(|p| p["clonePath"].as_str().or(p["name"].as_str()))
        .unwrap_or("project")
        .to_string();

    // Components.
    let mut primary: Option<String> = None;
    let mut services = Map::new();
    let mut env = Map::new();
    for c in d["components"].as_array().into_iter().flatten() {
        let name = c["name"].as_str().unwrap_or("component");
        if let Some(ct) = c.get("container") {
            let cenv: Map<String, Value> = ct["env"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|e| {
                    Some((
                        e["name"].as_str()?.to_string(),
                        json!(rewrite(e["value"].as_str().unwrap_or(""), &first_project)),
                    ))
                })
                .collect();
            let mounts = ct["mountSources"].as_bool().unwrap_or(true);
            if primary.is_none() && mounts {
                primary = Some(name.to_string());
                if let Some(img) = ct["image"].as_str() {
                    spec.insert("base".into(), json!(img));
                }
                env.extend(cenv);
            } else {
                let mut svc = Map::new();
                svc.insert("image".into(), ct["image"].clone());
                if !cenv.is_empty() {
                    svc.insert("env".into(), Value::Object(cenv));
                }
                let ports: Vec<Value> = ct["endpoints"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|e| e["targetPort"].as_u64())
                    .map(|p| json!(format!("{p}:{p}")))
                    .collect();
                if !ports.is_empty() {
                    svc.insert("ports".into(), Value::Array(ports));
                }
                services.insert(name.to_string(), Value::Object(svc));
            }
        } else if c.get("kubernetes").is_some()
            || c.get("openshift").is_some()
            || c.get("image").is_some()
        {
            warnings.push(format!("W3003: devfile component `{name}` (kubernetes/openshift/image) is not supported; ignored"));
        }
    }
    if !env.is_empty() {
        spec.insert("env".into(), Value::Object(env));
    }
    if !services.is_empty() {
        spec.insert("services".into(), Value::Object(services));
    }

    // Projects.
    let mut repos = Vec::new();
    for p in &projects {
        let name = p["name"].as_str().unwrap_or("project");
        let Some(git) = p.get("git") else {
            warnings.push(format!(
                "W3005: devfile project `{name}` is not a git project; ignored"
            ));
            continue;
        };
        let remotes = git["remotes"].as_object();
        let remote_name = git["checkoutFrom"]["remote"].as_str();
        let url = remote_name
            .and_then(|r| remotes.and_then(|m| m.get(r)))
            .or_else(|| remotes.and_then(|m| m.values().next()))
            .and_then(Value::as_str);
        let Some(url) = url else {
            warnings.push(format!(
                "W3005: devfile project `{name}` has no git remote; ignored"
            ));
            continue;
        };
        let mut r = json!({"url": url, "path": p["clonePath"].as_str().unwrap_or(name)});
        if let Some(rev) = git["checkoutFrom"]["revision"].as_str() {
            r["ref"] = json!(rev);
        }
        repos.push(r);
    }
    if !repos.is_empty() {
        spec.insert("workspace".into(), json!({"repos": repos}));
    }

    // Commands → hooks.
    let commands: Map<String, Value> = d["commands"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| Some((c["id"].as_str()?.to_lowercase(), c.clone())))
        .collect();
    fn expand(
        id: &str,
        cmds: &Map<String, Value>,
        first: &str,
        depth: usize,
        out: &mut Vec<String>,
    ) {
        let Some(c) = cmds.get(&id.to_lowercase()) else {
            return;
        };
        if depth > 8 {
            return;
        }
        if let Some(e) = c.get("exec") {
            let line = rewrite(e["commandLine"].as_str().unwrap_or(""), first);
            match e["workingDir"].as_str() {
                Some(w) => out.push(format!(
                    "cd \"{}\" && {line}",
                    rewrite(w, first).replace('"', "\\\"")
                )),
                None => out.push(line),
            }
        } else if let Some(comp) = c.get("composite") {
            for sub in comp["commands"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                expand(sub, cmds, first, depth + 1, out);
            }
        }
    }
    let mut build = Vec::new();
    let mut workspace = Vec::new();
    for id in d["events"]["preStart"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        expand(id, &commands, &first_project, 0, &mut build);
    }
    for (id, c) in &commands {
        if c["exec"]["group"]["kind"] == "build" || c["composite"]["group"]["kind"] == "build" {
            expand(id, &commands, &first_project, 0, &mut workspace);
        }
    }
    for id in d["events"]["postStart"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        expand(id, &commands, &first_project, 0, &mut workspace);
    }
    let mut hooks = Map::new();
    if !build.is_empty() {
        hooks.insert("build".into(), json!(build));
    }
    if !workspace.is_empty() {
        hooks.insert("workspace".into(), json!(workspace));
    }
    if !hooks.is_empty() {
        spec.insert("hooks".into(), Value::Object(hooks));
    }
    (Value::Object(spec), warnings)
}

/// The layer as a spanned node (spans point at the devfile).
pub fn to_node(devfile: &Node) -> (Node, Vec<Diagnostic>) {
    let (spec, warnings) = to_spec(&devfile.to_json());
    let mut node = Node::from_json(&spec);
    let span: Option<Span> = devfile.span.clone();
    node.span = span.clone();
    let diags = warnings
        .into_iter()
        .map(|w| {
            let (code, msg) = w.split_once(": ").unwrap_or(("W3003", w.as_str()));
            let code: &'static str = match code {
                "W3004" => "W3004",
                "W3005" => "W3005",
                _ => "W3003",
            };
            Diagnostic::warning(code, msg.to_string()).at(span.clone())
        })
        .collect();
    (node, diags)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_a_devfile() {
        let d = json!({
            "schemaVersion": "2.2.0",
            "metadata": {"name": "app"},
            "projects": [{"name": "web", "git": {"remotes": {"origin": "https://example.com/web.git"}, "checkoutFrom": {"revision": "main"}}}],
            "components": [
                {"name": "tools", "container": {"image": "quay.io/devfile/universal-developer-image:latest", "env": [{"name": "GOPATH", "value": "${PROJECTS_ROOT}/go"}]}},
                {"name": "db", "container": {"image": "postgres:16", "mountSources": false, "env": [{"name": "POSTGRES_PASSWORD", "value": "x"}], "endpoints": [{"name": "pg", "targetPort": 5432}]}},
                {"name": "k", "kubernetes": {"uri": "x.yaml"}}
            ],
            "commands": [
                {"id": "install", "exec": {"component": "tools", "commandLine": "npm ci", "workingDir": "${PROJECT_SOURCE}", "group": {"kind": "build", "isDefault": true}}},
                {"id": "seed", "exec": {"component": "tools", "commandLine": "echo seed"}},
                {"id": "setup", "exec": {"component": "tools", "commandLine": "echo setup"}},
                {"id": "start", "composite": {"commands": ["seed"]}}
            ],
            "events": {"preStart": ["setup"], "postStart": ["start"]}
        });
        let (s, w) = to_spec(&d);
        assert_eq!(
            s["base"],
            "quay.io/devfile/universal-developer-image:latest"
        );
        assert_eq!(s["env"]["GOPATH"], "{{ workspace.root }}/go");
        assert_eq!(s["services"]["db"]["image"], "postgres:16");
        assert_eq!(s["services"]["db"]["ports"], json!(["5432:5432"]));
        assert_eq!(
            s["workspace"]["repos"],
            json!([{"url": "https://example.com/web.git", "path": "web", "ref": "main"}])
        );
        assert_eq!(s["hooks"]["build"], json!(["echo setup"]));
        assert_eq!(
            s["hooks"]["workspace"],
            json!(["cd \"{{ workspace.root }}/web\" && npm ci", "echo seed"])
        );
        assert_eq!(w.len(), 1);
        assert!(w[0].starts_with("W3003"));
    }
}

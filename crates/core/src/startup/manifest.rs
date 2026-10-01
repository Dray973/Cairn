//! The parts of an `AppxManifest.xml` that describe a package's startup tasks.
//!
//! Manifests are machine-validated, well-formed XML, so a small scanner over start tags,
//! end tags and text is enough; namespace prefixes are ignored and only local names are
//! compared.

/// Package-level names and the startup tasks a manifest declares.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct Manifest {
    /// `Package/Properties/DisplayName`, possibly an `ms-resource:` reference.
    pub display_name: Option<String>,
    /// `Package/Properties/PublisherDisplayName`, possibly an `ms-resource:` reference.
    pub publisher_display_name: Option<String>,
    pub startup_tasks: Vec<StartupTaskDecl>,
}

/// One `StartupTask` element inside a `windows.startupTask` extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StartupTaskDecl {
    pub task_id: String,
    /// Executable relative to the package root: the extension's own `Executable`, else the
    /// enclosing application's.
    pub executable: Option<String>,
    /// `DisplayName` attribute, possibly an `ms-resource:` reference.
    pub display_name: Option<String>,
}

impl Manifest {
    pub fn task(&self, task_id: &str) -> Option<&StartupTaskDecl> {
        self.startup_tasks.iter().find(|t| t.task_id == task_id)
    }
}

/// Parses the fields of [`Manifest`] out of the manifest text.
pub(super) fn parse(xml: &str) -> Manifest {
    struct Frame {
        name: String,
        executable: Option<String>,
        startup_extension: bool,
    }

    let mut manifest = Manifest::default();
    let mut stack: Vec<Frame> = Vec::new();
    let tokens = Tokens { rest: xml };
    for token in tokens {
        match token {
            Token::Start {
                name,
                attrs,
                self_closing,
            } => {
                let name = local_name(name);
                let attrs = attributes(attrs);
                let attr = |key: &str| {
                    attrs
                        .iter()
                        .find(|(k, _)| local_name(k) == key)
                        .map(|(_, v)| v.clone())
                };
                if name == "StartupTask" {
                    let extension = stack.iter().rev().find(|f| f.startup_extension);
                    if let (Some(extension), Some(task_id)) = (extension, attr("TaskId")) {
                        let executable = extension.executable.clone().or_else(|| {
                            stack
                                .iter()
                                .rev()
                                .find(|f| f.name == "Application")
                                .and_then(|f| f.executable.clone())
                        });
                        manifest.startup_tasks.push(StartupTaskDecl {
                            task_id,
                            executable,
                            display_name: attr("DisplayName").filter(|v| !v.is_empty()),
                        });
                    }
                }
                if !self_closing {
                    stack.push(Frame {
                        startup_extension: name == "Extension"
                            && attr("Category").as_deref() == Some("windows.startupTask"),
                        executable: attr("Executable").filter(|v| !v.is_empty()),
                        name: name.to_string(),
                    });
                }
            }
            Token::End { name } => {
                let name = local_name(name);
                if let Some(pos) = stack.iter().rposition(|f| f.name == name) {
                    stack.truncate(pos);
                }
            }
            Token::Text(text) => {
                let path: Vec<&str> = stack.iter().map(|f| f.name.as_str()).collect();
                let slot = match path.as_slice() {
                    ["Package", "Properties", "DisplayName"] => &mut manifest.display_name,
                    ["Package", "Properties", "PublisherDisplayName"] => {
                        &mut manifest.publisher_display_name
                    }
                    _ => continue,
                };
                let text = decode_entities(text.trim());
                if !text.is_empty() {
                    *slot = Some(text);
                }
            }
        }
    }
    manifest
}

fn local_name(qualified: &str) -> &str {
    qualified.rsplit(':').next().unwrap_or(qualified)
}

#[derive(Debug, PartialEq, Eq)]
enum Token<'a> {
    Start {
        name: &'a str,
        attrs: &'a str,
        self_closing: bool,
    },
    End {
        name: &'a str,
    },
    Text(&'a str),
}

/// Start tags, end tags and text of an XML document; comments, processing instructions and
/// declarations are skipped, CDATA sections are returned as text.
struct Tokens<'a> {
    rest: &'a str,
}

impl<'a> Tokens<'a> {
    /// Drops everything up to and including `end`, or the rest when it is missing.
    fn skip_past(&mut self, end: &str) {
        self.rest = match self.rest.find(end) {
            Some(pos) => &self.rest[pos + end.len()..],
            None => "",
        };
    }
}

impl<'a> Iterator for Tokens<'a> {
    type Item = Token<'a>;

    fn next(&mut self) -> Option<Token<'a>> {
        loop {
            if self.rest.is_empty() {
                return None;
            }
            if !self.rest.starts_with('<') {
                let end = self.rest.find('<').unwrap_or(self.rest.len());
                let (text, rest) = self.rest.split_at(end);
                self.rest = rest;
                return Some(Token::Text(text));
            }
            if self.rest.starts_with("<!--") {
                self.skip_past("-->");
                continue;
            }
            if let Some(body) = self.rest.strip_prefix("<![CDATA[") {
                let end = body.find("]]>").unwrap_or(body.len());
                let text = &body[..end];
                self.rest = body.get(end + 3..).unwrap_or("");
                return Some(Token::Text(text));
            }
            if self.rest.starts_with("<?") {
                self.skip_past("?>");
                continue;
            }
            if self.rest.starts_with("<!") {
                self.skip_past(">");
                continue;
            }
            if let Some(body) = self.rest.strip_prefix("</") {
                let end = body.find('>').unwrap_or(body.len());
                let name = body[..end].trim();
                self.rest = body.get(end + 1..).unwrap_or("");
                return Some(Token::End { name });
            }

            // Start tag: find the closing '>' outside quoted attribute values.
            let body = &self.rest[1..];
            let mut quote = None;
            let mut end = body.len();
            for (i, c) in body.char_indices() {
                match (quote, c) {
                    (None, '"' | '\'') => quote = Some(c),
                    (Some(q), _) if c == q => quote = None,
                    (None, '>') => {
                        end = i;
                        break;
                    }
                    _ => {}
                }
            }
            let mut tag = &body[..end];
            self.rest = body.get(end + 1..).unwrap_or("");
            let self_closing = tag.ends_with('/');
            if self_closing {
                tag = &tag[..tag.len() - 1];
            }
            let name_end = tag.find(|c: char| c.is_whitespace()).unwrap_or(tag.len());
            return Some(Token::Start {
                name: &tag[..name_end],
                attrs: &tag[name_end..],
                self_closing,
            });
        }
    }
}

/// `(name, decoded value)` pairs of a start tag's attribute text.
fn attributes(mut s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    loop {
        s = s.trim_start();
        let Some(eq) = s.find('=') else {
            return out;
        };
        let name = s[..eq].trim();
        let after = s[eq + 1..].trim_start();
        let Some(quote) = after.chars().next().filter(|c| *c == '"' || *c == '\'') else {
            return out;
        };
        let value_start = &after[1..];
        let Some(close) = value_start.find(quote) else {
            return out;
        };
        out.push((name.to_string(), decode_entities(&value_start[..close])));
        s = &value_start[close + 1..];
    }
}

/// Replaces the predefined XML entities and numeric character references.
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp..];
        let decoded = after.find(';').and_then(|semi| {
            let entity = &after[1..semi];
            let c = match entity {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ => {
                    let code = if let Some(hex) = entity
                        .strip_prefix("#x")
                        .or_else(|| entity.strip_prefix("#X"))
                    {
                        u32::from_str_radix(hex, 16).ok()
                    } else {
                        entity.strip_prefix('#').and_then(|dec| dec.parse().ok())
                    };
                    code.and_then(char::from_u32)
                }
            };
            c.map(|c| (c, semi + 1))
        });
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &after[len..];
            }
            None => {
                out.push('&');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<!-- generated -->
<Package xmlns="http://schemas.microsoft.com/appx/manifest/foundation/windows10"
         xmlns:uap5="http://schemas.microsoft.com/appx/manifest/uap/windows10/5"
         xmlns:desktop="http://schemas.microsoft.com/appx/manifest/desktop/windows10">
  <Identity Name="Contoso.App" Publisher="CN=Contoso" Version="1.0.0.0" />
  <Properties>
    <DisplayName>Contoso &amp; Co &#x41;pp</DisplayName>
    <PublisherDisplayName>
      Contoso Ltd
    </PublisherDisplayName>
    <Logo>Assets\Logo.png</Logo>
  </Properties>
  <Applications>
    <Application Id="App" Executable="Main\App.exe" EntryPoint="Windows.FullTrustApplication">
      <uap:VisualElements DisplayName="Not the package name" Description="a > b" />
      <Extensions>
        <desktop:Extension Category="windows.startupTask" Executable="Helper\Start.exe" EntryPoint="Windows.FullTrustApplication">
          <desktop:StartupTask TaskId="HelperTask" Enabled="false" DisplayName="Contoso Helper" />
        </desktop:Extension>
        <uap5:Extension Category="windows.startupTask">
          <uap5:StartupTask TaskId="MainTask" Enabled="true" DisplayName="ms-resource:AppName"/>
        </uap5:Extension>
        <uap5:Extension Category="windows.protocol">
          <uap5:StartupTask TaskId="NotAStartupExtension" />
        </uap5:Extension>
      </Extensions>
    </Application>
    <Application Id="Other" Executable="Other.exe">
      <Extensions>
        <uap5:Extension Category='windows.startupTask'>
          <uap5:StartupTask TaskId='OtherTask'></uap5:StartupTask>
        </uap5:Extension>
      </Extensions>
    </Application>
  </Applications>
  <Extensions><![CDATA[ignored <text>]]></Extensions>
</Package>
"#;

    #[test]
    fn startup_tasks_and_package_names_are_read() {
        let manifest = parse(MANIFEST);
        assert_eq!(manifest.display_name.as_deref(), Some("Contoso & Co App"));
        assert_eq!(
            manifest.publisher_display_name.as_deref(),
            Some("Contoso Ltd")
        );
        assert_eq!(
            manifest.startup_tasks,
            [
                StartupTaskDecl {
                    task_id: "HelperTask".into(),
                    executable: Some(r"Helper\Start.exe".into()),
                    display_name: Some("Contoso Helper".into()),
                },
                StartupTaskDecl {
                    task_id: "MainTask".into(),
                    executable: Some(r"Main\App.exe".into()),
                    display_name: Some("ms-resource:AppName".into()),
                },
                StartupTaskDecl {
                    task_id: "OtherTask".into(),
                    executable: Some("Other.exe".into()),
                    display_name: None,
                },
            ]
        );
        assert_eq!(manifest.task("MainTask").unwrap().task_id, "MainTask");
        assert!(manifest.task("NotAStartupExtension").is_none());
    }

    #[test]
    fn malformed_input_does_not_panic() {
        for xml in [
            "",
            "<",
            "<Package",
            "<Package a=\"unterminated",
            "</",
            "<!-- open",
            "<![CDATA[ open",
            "<?xml",
            "text only",
            "<a b=c d='e'>&amp</a>",
        ] {
            let _ = parse(xml);
        }
        assert_eq!(parse("").startup_tasks, []);
    }

    #[test]
    fn entities_decode_and_unknown_ones_stay() {
        assert_eq!(
            decode_entities("a&lt;b&gt;c&quot;&apos;&#65;&#x42;"),
            "a<b>c\"'AB"
        );
        assert_eq!(decode_entities("&unknown; & &#xZZ;"), "&unknown; & &#xZZ;");
        assert_eq!(decode_entities("plain"), "plain");
    }

    #[test]
    fn attributes_accept_both_quote_styles() {
        assert_eq!(
            attributes(r#" a="1" b = '2 > 3'  c="x&amp;y""#),
            [
                ("a".to_string(), "1".to_string()),
                ("b".to_string(), "2 > 3".to_string()),
                ("c".to_string(), "x&y".to_string()),
            ]
        );
    }
}

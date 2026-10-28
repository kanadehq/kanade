//! Bounded, non-extracting reader for the payload-only productbuild contract.
//! OS validators are used only after this reader has rejected executable
//! installer metadata, links, extra members and unbounded compressed streams.
use std::collections::BTreeMap;
use std::io::Read;

use flate2::bufread::GzDecoder;
use flate2::read::ZlibDecoder;
use roxmltree::{Document, Node};

use super::{Error, Result};
use crate::native_install::macho;

pub const APP: &str = "Kanade Client.app";
pub const BUNDLE: &str = "com.yukimemi.kanade-client";
pub const COMPONENT: &str = "com.yukimemi.kanade-client.pkg";
pub const PRODUCT: &str = "com.yukimemi.kanade-client.product";
pub const PART: &str = "kanade-client-component.pkg";
pub const MAX_BYTES: usize = 256 * 1024 * 1024;
pub const EXE: &str = "Contents/MacOS/kanade-client";
pub const DIRS: &[&str] = &[
    "Contents",
    "Contents/MacOS",
    "Contents/Resources",
    "Contents/_CodeSignature",
];
pub const FILES: &[&str] = &[
    "Contents/Info.plist",
    EXE,
    "Contents/Resources/icon.icns",
    "Contents/Resources/THIRD-PARTY-NOTICES.md",
    "Contents/_CodeSignature/CodeResources",
];

pub(super) fn require(ok: bool, why: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(why.into()))
    }
}
fn xml(bytes: &[u8]) -> Result<Document<'_>> {
    let s = std::str::from_utf8(bytes).map_err(|_| Error::Invalid("XML encoding".into()))?;
    require(
        !s.contains("<!ENTITY") && !s.contains("<!DOCTYPE"),
        "XML declarations forbidden",
    )?;
    Document::parse(s).map_err(|e| Error::Invalid(format!("XML: {e}")))
}
fn one<'a, 'i>(n: Node<'a, 'i>, tag: &str) -> Result<Node<'a, 'i>> {
    let mut children = n.children().filter(|c| c.has_tag_name(tag));
    let out = children
        .next()
        .ok_or_else(|| Error::Invalid(format!("missing {tag}")))?;
    require(children.next().is_none(), "duplicate XML field")?;
    Ok(out)
}
fn value<'a, 'i: 'a>(n: Node<'a, 'i>, tag: &str) -> Result<&'a str> {
    one(n, tag)?
        .text()
        .ok_or_else(|| Error::Invalid(format!("empty {tag}")))
}
fn number(n: Node<'_, '_>, tag: &str) -> Result<usize> {
    value(n, tag)?
        .parse()
        .map_err(|_| Error::Invalid("invalid number".into()))
}
fn inflate(raw: &[u8], expected: usize) -> Result<Vec<u8>> {
    require(expected <= MAX_BYTES, "expansion limit")?;
    let mut decoder = ZlibDecoder::new(raw);
    let mut out = Vec::new();
    decoder
        .by_ref()
        .take(expected as u64 + 1)
        .read_to_end(&mut out)?;
    require(
        out.len() == expected && decoder.total_in() as usize == raw.len(),
        "zlib length/tail",
    )?;
    Ok(out)
}

/// XAR is parsed in memory. No archive extractor sees unvalidated member names.
pub(super) fn xar(bytes: &[u8]) -> Result<BTreeMap<String, Vec<u8>>> {
    require(bytes.len() >= 28 && bytes.len() <= MAX_BYTES, "XAR size")?;
    require(&bytes[..8] == b"xar!\0\x1c\0\x01", "XAR header")?;
    let compressed = u64::from_be_bytes(bytes[8..16].try_into().unwrap());
    let expanded = u64::from_be_bytes(bytes[16..24].try_into().unwrap());
    require(
        (1..=1024 * 1024).contains(&compressed) && (1..=4 * 1024 * 1024).contains(&expanded),
        "XAR TOC limit",
    )?;
    require(
        u32::from_be_bytes(bytes[24..28].try_into().unwrap()) <= 2,
        "XAR checksum kind",
    )?;
    let heap = 28 + compressed as usize;
    require(heap <= bytes.len(), "truncated TOC")?;
    let decoded = inflate(&bytes[28..heap], expanded as usize)?;
    let doc = xml(&decoded)?;
    require(doc.root_element().has_tag_name("xar"), "XAR root")?;
    require(
        !doc.descendants()
            .any(|n| n.has_tag_name("signature") || n.has_tag_name("link")),
        "unsigned regular XAR required",
    )?;
    let toc = one(doc.root_element(), "toc")?;
    let mut out = BTreeMap::new();
    let mut total = 0;
    let mut ranges = Vec::new();
    fn record(
        n: Node<'_, '_>,
        bytes: &[u8],
        heap: usize,
        total: &mut usize,
        ranges: &mut Vec<(usize, usize)>,
    ) -> Result<Vec<u8>> {
        let offset = number(n, "offset")?;
        let len = number(n, "length")?;
        let size = number(n, "size")?;
        *total = total
            .checked_add(size)
            .ok_or_else(|| Error::Invalid("XAR total overflow".into()))?;
        require(*total <= MAX_BYTES, "XAR total limit")?;
        let start = heap
            .checked_add(offset)
            .ok_or_else(|| Error::Invalid("XAR offset overflow".into()))?;
        let end = start
            .checked_add(len)
            .ok_or_else(|| Error::Invalid("XAR length overflow".into()))?;
        require(end <= bytes.len(), "XAR range")?;
        require(
            ranges.iter().all(|(a, b)| end <= *a || start >= *b),
            "overlapping XAR streams",
        )?;
        ranges.push((start, end));
        let raw = &bytes[start..end];
        match one(n, "encoding")?.attribute("style") {
            Some("application/octet-stream") => {
                require(len == size, "XAR raw size")?;
                Ok(raw.to_vec())
            }
            Some("application/x-gzip") => inflate(raw, size),
            _ => Err(Error::Invalid("XAR encoding".into())),
        }
    }
    fn visit(
        n: Node<'_, '_>,
        prefix: &str,
        bytes: &[u8],
        heap: usize,
        out: &mut BTreeMap<String, Vec<u8>>,
        total: &mut usize,
        ranges: &mut Vec<(usize, usize)>,
    ) -> Result<()> {
        for f in n.children().filter(|n| n.has_tag_name("file")) {
            let leaf = value(f, "name").or_else(|_| {
                // productbuild repeats identical <name> elements.
                let names: Vec<_> = f.children().filter(|n| n.has_tag_name("name")).collect();
                require(
                    !names.is_empty() && names.iter().all(|n| n.text() == names[0].text()),
                    "ambiguous XAR name",
                )?;
                names[0]
                    .text()
                    .ok_or_else(|| Error::Invalid("empty XAR name".into()))
            })?;
            require(!leaf.contains('/') && !leaf.is_empty(), "XAR leaf")?;
            let name = format!("{prefix}{leaf}");
            let dir = name == PART;
            require(
                dir || name == "Distribution"
                    || ["Bom", "Payload", "PackageInfo"]
                        .iter()
                        .any(|s| name == format!("{PART}/{s}")),
                "unexpected XAR member",
            )?;
            require(!out.contains_key(&name), "duplicate XAR member")?;
            require(
                value(f, "type")? == if dir { "directory" } else { "file" }
                    && one(f, "type")?.attributes().len() == 0,
                "XAR type/link",
            )?;
            let data = if dir {
                require(
                    !f.children().any(|n| n.has_tag_name("data")),
                    "directory data",
                )?;
                vec![]
            } else {
                require(
                    !f.children().any(|n| n.has_tag_name("file")),
                    "file children",
                )?;
                record(one(f, "data")?, bytes, heap, total, ranges)?
            };
            let eas: Vec<_> = f.children().filter(|n| n.has_tag_name("ea")).collect();
            require(eas.len() <= 1, "XAR xattr count")?;
            for ea in eas {
                require(
                    value(ea, "name")? == "com.apple.provenance" && number(ea, "size")? <= 4096,
                    "XAR xattr",
                )?;
                record(ea, bytes, heap, total, ranges)?;
            }
            out.insert(name.clone(), data);
            visit(f, &format!("{name}/"), bytes, heap, out, total, ranges)?;
        }
        Ok(())
    }
    visit(toc, "", bytes, heap, &mut out, &mut total, &mut ranges)?;
    require(out.len() == 5, "incomplete XAR")?;
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Entry {
    pub mode: u32,
    pub data: Vec<u8>,
}
pub(super) type Payload = BTreeMap<String, Entry>;

pub(super) fn expected_modes() -> BTreeMap<String, u32> {
    let mut out = BTreeMap::from([(String::new(), 0o40755), (APP.into(), 0o40755)]);
    for dir in DIRS {
        out.insert(format!("{APP}/{dir}"), 0o40755);
    }
    for file in FILES {
        out.insert(
            format!("{APP}/{file}"),
            if *file == EXE { 0o100755 } else { 0o100644 },
        );
    }
    out
}
fn octal(b: &[u8]) -> Result<u32> {
    require(b.iter().all(|b| (b'0'..=b'7').contains(b)), "cpio octal")?;
    u32::from_str_radix(std::str::from_utf8(b).unwrap(), 8)
        .map_err(|_| Error::Invalid("cpio integer".into()))
}
pub(super) fn payload(raw: &[u8]) -> Result<Payload> {
    let mut decoder = GzDecoder::new(raw);
    let mut bytes = Vec::new();
    decoder
        .by_ref()
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    require(
        bytes.len() <= MAX_BYTES && decoder.get_ref().is_empty(),
        "gzip limit/tail",
    )?;
    let mut rest = bytes.as_slice();
    fn take<'a>(rest: &mut &'a [u8], n: usize) -> Result<&'a [u8]> {
        require(rest.len() >= n, "truncated cpio")?;
        let (a, b) = rest.split_at(n);
        *rest = b;
        Ok(a)
    }
    let mut entries: Payload = BTreeMap::new();
    let mut links = BTreeMap::new();
    loop {
        let h = take(&mut rest, 76)?;
        require(&h[..6] == b"070707", "odc cpio required")?;
        let mut at = 6;
        let mut fields = Vec::new();
        for width in [6, 6, 6, 6, 6, 6, 6, 11, 6, 11] {
            fields.push(octal(&h[at..at + width])?);
            at += width;
        }
        let (mode, uid, gid, nlink, namesize, size) = (
            fields[2],
            fields[3],
            fields[4],
            fields[5],
            fields[8] as usize,
            fields[9] as usize,
        );
        require(
            (1..=4096).contains(&namesize) && size <= MAX_BYTES,
            "cpio size",
        )?;
        let name = take(&mut rest, namesize)?;
        require(
            name.last() == Some(&0) && !name[..name.len() - 1].contains(&0),
            "cpio name terminator",
        )?;
        let name = std::str::from_utf8(&name[..name.len() - 1])
            .map_err(|_| Error::Invalid("cpio name UTF8".into()))?;
        if name == "TRAILER!!!" {
            require(
                size == 0 && rest.len() <= 1024 * 1024 && rest.iter().all(|b| *b == 0),
                "cpio trailer",
            )?;
            break;
        }
        let name = if name == "." {
            ""
        } else {
            name.strip_prefix("./")
                .ok_or_else(|| Error::Invalid("cpio relative path".into()))?
        };
        require(
            name.is_empty()
                || name.split('/').all(|p| {
                    !p.is_empty() && p != "." && p != ".." && !p.chars().any(char::is_control)
                }),
            "cpio normalized path",
        )?;
        let directory = mode & 0o170000 == 0o40000;
        let carrier = name.rsplit('/').next().unwrap().starts_with("._");
        require(
            directory || mode & 0o170000 == 0o100000,
            "cpio link/special",
        )?;
        require(
            uid == 0 && gid == 0 && (directory || carrier || nlink == 1),
            "cpio owner/hardlink",
        )?;
        let executable = name == format!("{APP}/{EXE}");
        require(
            mode & 0o7777
                == if directory || executable {
                    0o755
                } else {
                    0o644
                },
            "cpio mode",
        )?;
        require(!directory || size == 0, "directory size")?;
        require(
            !carrier || (!directory && size <= 4096),
            "AppleDouble carrier",
        )?;
        require(
            entries.len() < 66 && !entries.contains_key(name),
            "cpio duplicate/count",
        )?;
        entries.insert(
            name.into(),
            Entry {
                mode,
                data: take(&mut rest, size)?.to_vec(),
            },
        );
        links.insert(name.to_owned(), nlink);
    }
    let carriers: Vec<String> = entries
        .keys()
        .filter(|n| n.rsplit('/').next().unwrap().starts_with("._"))
        .cloned()
        .collect();
    for carrier in carriers {
        let (head, leaf) = carrier
            .rsplit_once('/')
            .map_or(("", carrier.as_str()), |(a, b)| (a, b));
        let target = if head.is_empty() {
            leaf[2..].to_owned()
        } else {
            format!("{head}/{}", &leaf[2..])
        };
        require(
            entries.contains_key(&target) && links[&carrier] == links[&target],
            "orphan AppleDouble",
        )?;
        // Narrow AppleDouble form emitted by current pkgbuild: empty Finder
        // info/resource fork and exactly one 11-byte provenance attribute.
        // Validate every structural byte, ignoring only the provenance value.
        let data = &entries[&carrier].data;
        let prefix: &[u8] = &[
            0, 5, 22, 7, 0, 2, 0, 0, 77, 97, 99, 32, 79, 83, 32, 88, 32, 32, 32, 32, 32, 32, 32,
            32, 0, 2, 0, 0, 0, 9, 0, 0, 0, 50, 0, 0, 0, 113, 0, 0, 0, 2, 0, 0, 0, 163, 0, 0, 0, 0,
        ];
        require(
            data.len() == 163
                && data.starts_with(prefix)
                && data[50..84].iter().all(|b| *b == 0)
                && &data[84..88] == b"ATTR"
                && data[88..120]
                    == [
                        0, 0, 0, 0, 0, 0, 0, 163, 0, 0, 0, 152, 0, 0, 0, 11, 0, 0, 0, 0, 0, 0, 0,
                        0, 0, 0, 0, 0, 0, 0, 0, 1,
                    ]
                && data[120..131] == [0, 0, 0, 152, 0, 0, 0, 11, 0, 0, 21]
                && &data[131..152] == b"com.apple.provenance\0",
            "AppleDouble attribute contract",
        )?;
        entries.remove(&carrier);
    }
    require(
        entries
            .iter()
            .map(|(n, e)| (n.clone(), e.mode))
            .collect::<BTreeMap<_, _>>()
            == expected_modes(),
        "payload fixed tree",
    )?;
    for (name, entry) in &entries {
        if entry.mode & 0o170000 == 0o100000 {
            require(
                !entry.data.is_empty() && !entry.data.starts_with(b"#!"),
                "empty/script payload",
            )?;
            if name == &format!("{APP}/{EXE}") {
                macho::check_arm64_executable(&entry.data)
                    .map_err(|e| Error::Invalid(e.to_string()))?;
            } else {
                require(
                    !entry.data.starts_with(b"\xcf\xfa\xed\xfe"),
                    "unexpected Mach-O",
                )?;
            }
        }
    }
    Ok(entries)
}

fn attrs(n: Node<'_, '_>, expected: &[(&str, &str)]) -> Result<()> {
    require(
        n.attributes().len() == expected.len()
            && expected.iter().all(|(k, v)| n.attribute(*k) == Some(*v)),
        "XML attributes differ from fixed contract",
    )
}
/// An allowlist, not a search for a few forbidden tags: Distribution attributes
/// can contain JavaScript even without <script> elements.
pub(super) fn distribution(bytes: &[u8], version: &str) -> Result<()> {
    let doc = xml(bytes)?;
    let root = doc.root_element();
    require(
        root.has_tag_name("installer-gui-script"),
        "Distribution root",
    )?;
    attrs(root, &[("minSpecVersion", "2")])?;
    let leaf = |n: Node<'_, '_>| {
        require(
            !n.children().any(|n| n.is_element()),
            "unexpected Distribution children",
        )
    };
    require(
        root.children().filter(Node::is_element).count() == 9,
        "Distribution children",
    )?;
    for tag in [
        "title",
        "options",
        "allowed-os-versions",
        "domains",
        "product",
        "choices-outline",
        "choice",
    ] {
        one(root, tag)?;
    }
    let refs: Vec<_> = root
        .children()
        .filter(|n| n.has_tag_name("pkg-ref"))
        .collect();
    require(refs.len() == 2, "productbuild reference count")?;
    for n in root.descendants().filter(Node::is_element).skip(1) {
        match n.tag_name().name() {
            "title" if n.parent() == Some(root) => {
                attrs(n, &[])?;
                leaf(n)?;
                require(n.text() == Some("Kanade Client"), "title")?;
            }
            "options" if n.parent() == Some(root) => {
                attrs(
                    n,
                    &[
                        ("customize", "never"),
                        ("require-scripts", "false"),
                        ("hostArchitectures", "arm64"),
                    ],
                )?;
                leaf(n)?;
            }
            "allowed-os-versions" if n.parent() == Some(root) => {
                attrs(n, &[])?;
                require(
                    n.children().filter(Node::is_element).count() == 1,
                    "OS constraints",
                )?;
                one(n, "os-version")?;
            }
            "os-version"
                if n.parent()
                    .is_some_and(|p| p.has_tag_name("allowed-os-versions")) =>
            {
                attrs(n, &[("min", "14.0")])?;
                leaf(n)?;
            }
            "domains" if n.parent() == Some(root) => {
                attrs(
                    n,
                    &[
                        ("enable_anywhere", "false"),
                        ("enable_currentUserHome", "false"),
                        ("enable_localSystem", "true"),
                    ],
                )?;
                leaf(n)?;
            }
            "product" if n.parent() == Some(root) => {
                attrs(n, &[("id", PRODUCT), ("version", version)])?;
                leaf(n)?;
            }
            "choices-outline" if n.parent() == Some(root) => {
                attrs(n, &[])?;
                require(
                    n.children().filter(Node::is_element).count() == 1,
                    "choices shape",
                )?;
                one(n, "line")?;
            }
            "line"
                if n.parent()
                    .is_some_and(|p| p.has_tag_name("choices-outline")) =>
            {
                attrs(n, &[("choice", "default")])?;
                leaf(n)?;
            }
            "choice" if n.parent() == Some(root) => {
                attrs(n, &[("id", "default"), ("title", "Kanade Client")])?;
                require(
                    n.children().filter(Node::is_element).count() == 1,
                    "choice shape",
                )?;
                one(n, "pkg-ref")?;
            }
            "pkg-ref" if n.parent().is_some_and(|p| p.has_tag_name("choice")) => {
                attrs(n, &[("id", COMPONENT)])?;
                leaf(n)?;
                require(
                    n.text().is_none_or(|t| t.trim().is_empty()),
                    "choice reference text",
                )?;
            }
            "pkg-ref" if n.parent() == Some(root) && n.children().any(|c| c.is_element()) => {
                attrs(n, &[("id", COMPONENT)])?;
                require(
                    n.text().is_none_or(|s| s.trim().is_empty()),
                    "metadata reference must not name another package",
                )?;
                require(
                    n.children().filter(Node::is_element).count() == 1,
                    "bundle version reference",
                )?;
                one(n, "bundle-version")?;
            }
            "pkg-ref" if n.parent() == Some(root) => {
                let kb = n.attribute("installKBytes").unwrap_or("");
                require(
                    kb.parse::<usize>().is_ok_and(|v| v <= MAX_BYTES / 1024),
                    "Distribution size",
                )?;
                attrs(
                    n,
                    &[
                        ("id", COMPONENT),
                        ("version", version),
                        ("installKBytes", kb),
                        ("updateKBytes", "0"),
                    ],
                )?;
                require(
                    n.text() == Some(&format!("#{PART}")),
                    "Distribution local reference",
                )?;
            }
            "bundle-version" if n.parent().is_some_and(|p| p.has_tag_name("pkg-ref")) => {
                attrs(n, &[])?;
                require(
                    n.children().filter(Node::is_element).count() == 1,
                    "Distribution bundle version",
                )?;
                one(n, "bundle")?;
            }
            "bundle" if n.parent().is_some_and(|p| p.has_tag_name("bundle-version")) => {
                attrs(
                    n,
                    &[
                        ("id", BUNDLE),
                        ("path", APP),
                        ("CFBundleVersion", version),
                        ("CFBundleShortVersionString", version),
                    ],
                )?;
                leaf(n)?;
            }
            _ => {
                return Err(Error::Invalid(
                    "unexpected Distribution element/nesting".into(),
                ));
            }
        }
    }
    require(
        refs.iter()
            .filter(|n| n.children().any(|c| c.is_element()))
            .count()
            == 1,
        "ambiguous Distribution locator",
    )
}

pub(super) fn package_info(bytes: &[u8], version: &str) -> Result<()> {
    let doc = xml(bytes)?;
    let root = doc.root_element();
    require(root.has_tag_name("pkg-info"), "PackageInfo root")?;
    for (k, v) in [
        ("identifier", COMPONENT),
        ("version", version),
        ("install-location", "/Applications"),
        ("relocatable", "false"),
        ("auth", "root"),
        ("format-version", "2"),
        ("overwrite-permissions", "true"),
    ] {
        require(
            root.attribute(k) == Some(v),
            "PackageInfo identity/location",
        )?;
    }
    for a in root.attributes() {
        require(
            [
                "identifier",
                "version",
                "install-location",
                "relocatable",
                "auth",
                "format-version",
                "generator-version",
                "overwrite-permissions",
                "postinstall-action",
            ]
            .contains(&a.name()),
            "PackageInfo attribute",
        )?;
    }
    require(
        root.attribute("postinstall-action")
            .is_none_or(|a| a == "none"),
        "postinstall action",
    )?;
    for n in root.descendants().filter(Node::is_element).skip(1) {
        require(
            [
                "payload",
                "bundle",
                "bundle-version",
                "upgrade-bundle",
                "update-bundle",
                "atomic-update-bundle",
                "strict-identifier",
                "relocate",
            ]
            .contains(&n.tag_name().name()),
            "PackageInfo executable/unknown element",
        )?;
        if n.has_tag_name("bundle") && n.parent() != Some(root) {
            attrs(n, &[("id", BUNDLE)])?;
        } else if n.has_tag_name("bundle") {
            require(
                n.attribute("id") == Some(BUNDLE)
                    && n.attribute("path")
                        .is_some_and(|p| p == format!("./{APP}") || p == APP)
                    && n.attribute("CFBundleVersion") == Some(version)
                    && n.attribute("CFBundleShortVersionString") == Some(version),
                "PackageInfo bundle identity",
            )?;
        }
    }
    let bundle = one(root, "bundle")?;
    require(
        bundle.attributes().len() == 4,
        "PackageInfo bundle attributes",
    )?;
    for tag in [
        "bundle-version",
        "upgrade-bundle",
        "update-bundle",
        "atomic-update-bundle",
        "strict-identifier",
        "relocate",
    ] {
        let node = one(root, tag)?;
        require(
            node.attributes().len() == 0,
            "PackageInfo container attributes",
        )?;
        let children: Vec<_> = node.children().filter(Node::is_element).collect();
        if tag == "upgrade-bundle" || tag == "strict-identifier" {
            require(
                children.len() == 1 && children[0].has_tag_name("bundle"),
                "PackageInfo bundle selection",
            )?;
        } else {
            require(
                children.is_empty(),
                "PackageInfo relocation/update behavior unsupported",
            )?;
        }
    }
    require(
        root.children().filter(Node::is_element).count() == 8,
        "PackageInfo child count",
    )?;
    let payload = one(root, "payload")?;
    require(
        payload.attributes().len() == 2,
        "PackageInfo payload attributes",
    )?;
    require(
        payload
            .attribute("numberOfFiles")
            .and_then(|s| s.parse::<usize>().ok())
            .is_some_and(|n| n <= 64)
            && payload
                .attribute("installKBytes")
                .and_then(|s| s.parse::<usize>().ok())
                .is_some_and(|n| n <= MAX_BYTES / 1024),
        "PackageInfo payload limits",
    )
}

pub(super) fn info_plist(bytes: &[u8], version: &str) -> Result<()> {
    // Apple XML plist's standard DOCTYPE is inert here; no DTD resolution.
    let text =
        std::str::from_utf8(bytes).map_err(|_| Error::Invalid("Info.plist encoding".into()))?;
    let text = text.replace("<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">","");
    let doc = xml(text.as_bytes())?;
    require(doc.root_element().has_tag_name("plist"), "plist root")?;
    let dict = one(doc.root_element(), "dict")?;
    let nodes: Vec<_> = dict.children().filter(Node::is_element).collect();
    require(nodes.len() % 2 == 0, "plist pairs")?;
    let mut values = BTreeMap::new();
    for pair in nodes.chunks_exact(2) {
        require(pair[0].has_tag_name("key"), "plist key")?;
        let key = pair[0].text().unwrap_or("");
        require(values.insert(key, pair[1]).is_none(), "duplicate plist key")?;
    }
    for (key, value) in [
        ("CFBundleIdentifier", BUNDLE),
        ("CFBundleName", "Kanade Client"),
        ("CFBundlePackageType", "APPL"),
        ("CFBundleExecutable", "kanade-client"),
        ("CFBundleVersion", version),
        ("CFBundleShortVersionString", version),
        ("LSMinimumSystemVersion", "14.0"),
    ] {
        require(
            values
                .get(key)
                .is_some_and(|n| n.has_tag_name("string") && n.text() == Some(value)),
            "Info.plist identity/version/minimum OS",
        )?;
    }
    require(
        !values.contains_key("CFBundleURLTypes"),
        "URL schemes unsupported",
    )
}

pub(super) fn bom(text: &str) -> Result<()> {
    let mut seen = BTreeMap::new();
    for line in text.lines() {
        let f: Vec<_> = line.split('\t').collect();
        require(
            f.len() == 4 && f[2] == "root" && f[3] == "wheel",
            "BOM format/owner",
        )?;
        let path = if f[0] == "." {
            ""
        } else {
            f[0].strip_prefix("./")
                .ok_or_else(|| Error::Invalid("BOM path".into()))?
        };
        let mode = u32::from_str_radix(f[1], 8).map_err(|_| Error::Invalid("BOM mode".into()))?;
        require(
            seen.insert(path.to_owned(), mode).is_none(),
            "duplicate BOM entry",
        )?;
    }
    let carriers: Vec<_> = seen
        .keys()
        .filter(|n| n.rsplit('/').next().unwrap().starts_with("._"))
        .cloned()
        .collect();
    for name in carriers {
        let (head, leaf) = name
            .rsplit_once('/')
            .map_or(("", name.as_str()), |(a, b)| (a, b));
        let target = if head.is_empty() {
            leaf[2..].to_owned()
        } else {
            format!("{head}/{}", &leaf[2..])
        };
        require(
            seen.get(&target) == seen.get(&name),
            "BOM AppleDouble target/mode",
        )?;
        seen.remove(&name);
    }
    require(
        seen == expected_modes(),
        "BOM differs from validated payload",
    )
}

/// POSIX cksum used by BOM regular-file records (not publisher authentication).
pub(super) fn cksum(bytes: &[u8]) -> u32 {
    const TABLE: [u32; 256] = {
        let mut table = [0; 256];
        let mut i = 0;
        while i < 256 {
            let mut value = (i as u32) << 24;
            let mut bit = 0;
            while bit < 8 {
                value = if value & 0x80000000 != 0 {
                    (value << 1) ^ 0x04c11db7
                } else {
                    value << 1
                };
                bit += 1;
            }
            table[i] = value;
            i += 1;
        }
        table
    };
    let update = |crc: u32, byte: u8| (crc << 8) ^ TABLE[((crc >> 24) as u8 ^ byte) as usize];
    let mut crc = 0;
    for byte in bytes {
        crc = update(crc, *byte);
    }
    let mut length = bytes.len();
    while length != 0 {
        crc = update(crc, length as u8);
        length >>= 8;
    }
    !crc
}
/// Compare BOM paths, modes, ownership, file sizes AND cksums with the actual
/// parsed cpio before installer runs. Carrier records describe xattrs and have
/// zero file size/checksum in BOM even when cpio contains an attribute table.
pub(super) fn bom_payload(text: &str, payload: &Payload) -> Result<()> {
    let mut metadata = String::new();
    for line in text.lines() {
        let f: Vec<_> = line.split('\t').collect();
        require(f.len() == 6, "BOM content row")?;
        metadata.push_str(&format!("{}\t{}\t{}\t{}\n", f[0], f[1], f[3], f[4]));
        let name = if f[0] == "." {
            ""
        } else {
            f[0].strip_prefix("./")
                .ok_or_else(|| Error::Invalid("BOM path".into()))?
        };
        if name.rsplit('/').next().unwrap().starts_with("._") {
            require(f[2] == "0" && f[5] == "0", "BOM carrier size/checksum")?;
        } else {
            let entry = payload
                .get(name)
                .ok_or_else(|| Error::Invalid("BOM unexpected path".into()))?;
            if entry.mode & 0o170000 == 0o40000 {
                require(f[2].is_empty() && f[5].is_empty(), "BOM directory data")?;
            } else {
                require(
                    f[2].parse::<usize>().ok() == Some(entry.data.len())
                        && f[5].parse::<u32>().ok() == Some(cksum(&entry.data)),
                    "BOM payload size/checksum mismatch",
                )?;
            }
        }
    }
    bom(&metadata)
}

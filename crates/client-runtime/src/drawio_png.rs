use crate::PreviewImageError;
use base64::{Engine, engine::general_purpose::STANDARD};
use flate2::read::{DeflateDecoder, ZlibDecoder};
use notryx_protocol::DrawioPngStatus;
use quick_xml::{Reader, events::Event};
use std::io::Read;

const MAX_RAW_XML: usize = 5 * 1024 * 1024;
const MAX_EXPANDED_XML: usize = 20 * 1024 * 1024;
const MAX_CELLS: usize = 5000;
const MAX_DEPTH: usize = 64;
type XmlResult<T> = Result<T, DrawioPngStatus>;

pub(crate) fn prepare_png(bytes: &[u8]) -> Result<(Vec<u8>, DrawioPngStatus), PreviewImageError> {
    if bytes.len() > 25 * 1024 * 1024 {
        return Err(PreviewImageError::TooLarge);
    }
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err(PreviewImageError::Invalid);
    }
    let mut output = bytes[..8].to_vec();
    let mut offset = 8;
    let mut status = DrawioPngStatus::None;
    let mut found = false;
    while offset < bytes.len() {
        let header = bytes
            .get(offset..offset + 8)
            .ok_or(PreviewImageError::Invalid)?;
        let length = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
        let end = offset
            .checked_add(12)
            .and_then(|n| n.checked_add(length))
            .filter(|end| *end <= bytes.len())
            .ok_or(PreviewImageError::Invalid)?;
        let kind = &header[4..8];
        let data = &bytes[offset + 8..end - 4];
        if matches!(kind, b"tEXt" | b"zTXt" | b"iTXt") {
            if data.starts_with(b"mxfile\0") || data == b"mxfile" {
                let checksum = u32::from_be_bytes(bytes[end - 4..end].try_into().unwrap());
                if found || crc32fast::hash(&bytes[offset + 4..end - 4]) != checksum {
                    status = DrawioPngStatus::InvalidXml;
                } else {
                    status = inspect_text(kind, data).unwrap_or_else(|error| error);
                }
                found = true;
            }
        } else {
            output.extend_from_slice(&bytes[offset..end]);
        }
        if kind == b"IEND" {
            return if length == 0 {
                Ok((output, status))
            } else {
                Err(PreviewImageError::Invalid)
            };
        }
        offset = end;
    }
    Err(PreviewImageError::Invalid)
}

fn inspect_text(kind: &[u8], data: &[u8]) -> XmlResult<DrawioPngStatus> {
    let content = data.get(7..).ok_or(DrawioPngStatus::InvalidXml)?;
    let xml = match kind {
        b"tEXt" => decode_uri(content, MAX_RAW_XML)?,
        b"zTXt" => {
            if content.first() != Some(&0) {
                return Err(DrawioPngStatus::InvalidXml);
            }
            let mut decoder = ZlibDecoder::new(&content[1..]);
            let xml = decode_uri(&mut decoder, MAX_RAW_XML)?;
            if decoder.total_in() != (content.len() - 1) as u64 {
                return Err(DrawioPngStatus::InvalidXml);
            }
            xml
        }
        _ => return Err(DrawioPngStatus::UnsupportedXml),
    };
    let mut budget = XmlBudget {
        bytes: xml.len(),
        cells: 0,
    };
    validate_xml(&xml, false, &mut budget)?;
    Ok(DrawioPngStatus::EmbeddedXml)
}

// Decode while reading so neither PNG metadata nor compressed pages can expand without a bound.
fn decode_uri(mut reader: impl Read, limit: usize) -> XmlResult<Vec<u8>> {
    let mut output = Vec::new();
    let mut buffer = [0; 4096];
    let mut encoded = None;
    let mut escape = 0;
    let mut high = 0;
    let mut consumed = 0usize;
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|_| DrawioPngStatus::InvalidXml)?;
        if count == 0 {
            break;
        }
        consumed += count;
        if consumed > limit.saturating_mul(3) {
            return Err(DrawioPngStatus::LimitExceeded);
        }
        for &byte in &buffer[..count] {
            if encoded.is_none() && !byte.is_ascii_whitespace() {
                encoded = Some(byte == b'%');
            }
            let byte = if encoded == Some(true) {
                match escape {
                    1 => {
                        high = hex(byte)?;
                        escape = 2;
                        continue;
                    }
                    2 => {
                        escape = 0;
                        high * 16 + hex(byte)?
                    }
                    _ if byte == b'%' => {
                        escape = 1;
                        continue;
                    }
                    _ => byte,
                }
            } else {
                byte
            };
            if output.len() == limit {
                return Err(DrawioPngStatus::LimitExceeded);
            }
            output.push(byte);
        }
    }
    if escape != 0 {
        return Err(DrawioPngStatus::InvalidXml);
    }
    Ok(output)
}

fn hex(byte: u8) -> XmlResult<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(DrawioPngStatus::InvalidXml),
    }
}

struct XmlBudget {
    bytes: usize,
    cells: usize,
}

fn allowed_image(value: &str) -> bool {
    value.is_empty()
        || [
            "data:image/png;base64,",
            "data:image/jpeg;base64,",
            "data:image/webp;base64,",
            "data:image/png,",
            "data:image/jpeg,",
            "data:image/webp,",
        ]
        .iter()
        .any(|prefix| value.starts_with(prefix))
}

fn inspect_attribute(name: &[u8], value: &str) -> XmlResult<()> {
    if value
        .chars()
        .any(|ch| ch < ' ' && !matches!(ch, '\t' | '\r' | '\n'))
    {
        return Err(DrawioPngStatus::InvalidXml);
    }
    let lower = value.to_ascii_lowercase();
    if matches!(name, b"image" | b"src" | b"href" | b"xlink:href") && !allowed_image(value.trim()) {
        return Err(DrawioPngStatus::UnsupportedXml);
    }
    if name == b"style" {
        let mut parts = value.split(';').peekable();
        while let Some(part) = parts.next() {
            if let Some((key, image)) = part.split_once('=')
                && key.trim().eq_ignore_ascii_case("image")
            {
                let image = image.trim();
                if matches!(
                    image,
                    "data:image/png" | "data:image/jpeg" | "data:image/webp"
                ) && parts.peek().is_some_and(|next| next.starts_with("base64,"))
                {
                    parts.next();
                } else if !allowed_image(image) {
                    return Err(DrawioPngStatus::UnsupportedXml);
                }
            }
        }
    }
    // HTML image labels need the isolated editor's resource policy before they can be reopened.
    if [
        "<img", "<iframe", "<object", "<embed", "<script", "<style", "<link", "<svg",
    ]
    .iter()
    .any(|tag| lower.contains(tag))
    {
        return Err(DrawioPngStatus::UnsupportedXml);
    }
    Ok(())
}

fn validate_xml(bytes: &[u8], graph_only: bool, budget: &mut XmlBudget) -> XmlResult<()> {
    let xml = std::str::from_utf8(bytes).map_err(|_| DrawioPngStatus::InvalidXml)?;
    if xml.chars().any(|ch| {
        (ch < ' ' && !matches!(ch, '\t' | '\r' | '\n')) || ch == '\u{fffe}' || ch == '\u{ffff}'
    }) {
        return Err(DrawioPngStatus::InvalidXml);
    }
    let mut reader = Reader::from_str(xml);
    reader.config_mut().expand_empty_elements = true;
    let mut stack: Vec<Vec<u8>> = Vec::new();
    let mut seen_document = false;
    let mut diagram_text = String::new();
    let mut diagram_model = false;
    let mut graph_root = false;
    let mut diagrams = 0;
    loop {
        match reader
            .read_event()
            .map_err(|_| DrawioPngStatus::InvalidXml)?
        {
            Event::Start(element) => {
                if stack.len() >= MAX_DEPTH {
                    return Err(DrawioPngStatus::LimitExceeded);
                }
                let name = element.name();
                let name = name.as_ref().as_bytes();
                let parent = stack.last().map(Vec::as_slice);
                if parent.is_none() {
                    if seen_document
                        || !(name == b"mxGraphModel" || (!graph_only && name == b"mxfile"))
                    {
                        return Err(DrawioPngStatus::InvalidXml);
                    }
                    seen_document = true;
                } else if (parent == Some(b"mxfile") && name != b"diagram")
                    || (parent == Some(b"diagram") && name != b"mxGraphModel")
                    || (parent == Some(b"mxGraphModel") && name != b"root")
                {
                    return Err(DrawioPngStatus::InvalidXml);
                }
                match name {
                    b"mxfile" if parent.is_some() => return Err(DrawioPngStatus::InvalidXml),
                    b"diagram" => {
                        if parent != Some(b"mxfile") {
                            return Err(DrawioPngStatus::InvalidXml);
                        }
                        diagrams += 1;
                        diagram_text.clear();
                        diagram_model = false;
                    }
                    b"mxGraphModel" => {
                        if parent.is_some() && parent != Some(b"diagram") {
                            return Err(DrawioPngStatus::InvalidXml);
                        }
                        if parent.is_some() {
                            if diagram_model || !diagram_text.trim().is_empty() {
                                return Err(DrawioPngStatus::InvalidXml);
                            }
                            diagram_model = true;
                        }
                        graph_root = false;
                    }
                    b"root" => {
                        if parent != Some(b"mxGraphModel") || graph_root {
                            return Err(DrawioPngStatus::InvalidXml);
                        }
                        graph_root = true;
                    }
                    b"mxCell" => {
                        budget.cells += 1;
                        if budget.cells > MAX_CELLS {
                            return Err(DrawioPngStatus::LimitExceeded);
                        }
                    }
                    _ => {}
                }
                for attribute in element.attributes() {
                    let attribute = attribute.map_err(|_| DrawioPngStatus::InvalidXml)?;
                    let value = attribute
                        .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                        .map_err(|_| DrawioPngStatus::InvalidXml)?;
                    inspect_attribute(attribute.key.as_ref().as_bytes(), &value)?;
                }
                stack.push(name.to_vec());
            }
            Event::End(element) => {
                let name = stack.pop().ok_or(DrawioPngStatus::InvalidXml)?;
                if element.name().as_ref().as_bytes() != name {
                    return Err(DrawioPngStatus::InvalidXml);
                }
                match name.as_slice() {
                    b"mxGraphModel" if !graph_root => return Err(DrawioPngStatus::InvalidXml),
                    b"mxfile" if diagrams == 0 => return Err(DrawioPngStatus::InvalidXml),
                    b"diagram" => {
                        let text = diagram_text.trim();
                        if text.is_empty() {
                            if !diagram_model {
                                return Err(DrawioPngStatus::InvalidXml);
                            }
                        } else {
                            if diagram_model {
                                return Err(DrawioPngStatus::InvalidXml);
                            }
                            let compressed = STANDARD
                                .decode(text)
                                .map_err(|_| DrawioPngStatus::InvalidXml)?;
                            let mut decoder = DeflateDecoder::new(compressed.as_slice());
                            let expanded = decode_uri(
                                &mut decoder,
                                MAX_EXPANDED_XML.saturating_sub(budget.bytes),
                            )?;
                            if decoder.total_in() != compressed.len() as u64 {
                                return Err(DrawioPngStatus::InvalidXml);
                            }
                            budget.bytes += expanded.len();
                            validate_xml(&expanded, true, budget)?;
                        }
                    }
                    _ => {}
                }
            }
            Event::Text(text) => {
                let text = text.xml10_content();
                if stack.last().is_some_and(|name| name == b"diagram") {
                    diagram_text.push_str(&text);
                } else if (stack.is_empty()
                    || stack
                        .last()
                        .is_some_and(|name| matches!(name.as_slice(), b"mxfile" | b"mxGraphModel")))
                    && !text.trim().is_empty()
                {
                    return Err(DrawioPngStatus::InvalidXml);
                }
            }
            Event::CData(_) => return Err(DrawioPngStatus::UnsupportedXml),
            Event::GeneralRef(reference) => {
                if stack.is_empty() || stack.last().is_some_and(|name| name == b"diagram") {
                    return Err(DrawioPngStatus::InvalidXml);
                }
                let value = format!("&{};", reference.xml10_content());
                quick_xml::escape::unescape(&value).map_err(|_| DrawioPngStatus::InvalidXml)?;
            }
            Event::DocType(_) | Event::PI(_) => return Err(DrawioPngStatus::UnsupportedXml),
            Event::Decl(declaration) => {
                if seen_document
                    || declaration
                        .version()
                        .map_err(|_| DrawioPngStatus::InvalidXml)?
                        != "1.0"
                {
                    return Err(DrawioPngStatus::UnsupportedXml);
                }
                if let Some(encoding) = declaration.encoding()
                    && !encoding
                        .map_err(|_| DrawioPngStatus::InvalidXml)?
                        .eq_ignore_ascii_case("utf-8")
                {
                    return Err(DrawioPngStatus::UnsupportedXml);
                }
            }
            Event::Comment(_) => {}
            Event::Eof => break,
            Event::Empty(_) => unreachable!(),
        }
    }
    if !seen_document || !stack.is_empty() {
        return Err(DrawioPngStatus::InvalidXml);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::STANDARD};
    use flate2::{
        Compression,
        write::{DeflateEncoder, ZlibEncoder},
    };
    use std::io::Write;

    const GRAPH: &str = "<mxGraphModel><root><mxCell id=\"0\"/><mxCell id=\"1\" parent=\"0\"/><mxCell id=\"2\" value=\"한글 https://example.invalid\" parent=\"1\"/></root></mxGraphModel>";

    fn png() -> Vec<u8> {
        STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==").unwrap()
    }

    fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut chunk = (data.len() as u32).to_be_bytes().to_vec();
        chunk.extend_from_slice(kind);
        chunk.extend_from_slice(data);
        chunk.extend_from_slice(&crc32fast::hash(&chunk[4..]).to_be_bytes());
        chunk
    }

    fn with_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut png = png();
        png.splice(33..33, chunk(kind, data));
        png
    }

    fn uri(value: &str) -> String {
        value
            .as_bytes()
            .iter()
            .map(|byte| format!("%{byte:02X}"))
            .collect()
    }

    fn text_xml(xml: &str) -> Vec<u8> {
        with_chunk(b"tEXt", format!("mxfile\0{}", uri(xml)).as_bytes())
    }

    fn status(xml: &str) -> DrawioPngStatus {
        prepare_png(&text_xml(xml)).unwrap().1
    }

    #[test]
    fn strips_text_from_the_preview_without_changing_the_original() {
        let source = text_xml(&format!("<mxfile><diagram>{GRAPH}</diagram></mxfile>"));
        let original = source.clone();
        let (preview, status) = prepare_png(&source).unwrap();
        assert_eq!(status, DrawioPngStatus::EmbeddedXml);
        assert_eq!(source, original);
        assert_eq!(preview, png());
        assert_eq!(image::load_from_memory(&preview).unwrap().width(), 1);
    }

    #[test]
    fn recognizes_missing_corrupt_and_duplicate_xml_without_losing_pixels() {
        assert_eq!(prepare_png(&png()).unwrap().1, DrawioPngStatus::None);
        for xml in [
            "<mxfile>",
            "<svg/>",
            "<mxfile/>",
            "<mxfile><diagram/></mxfile>",
        ] {
            let (preview, state) = prepare_png(&text_xml(xml)).unwrap();
            assert_eq!(state, DrawioPngStatus::InvalidXml, "{xml}");
            assert_eq!(preview, png());
        }
        let mut corrupt = text_xml(GRAPH);
        let crc_offset = 41 + u32::from_be_bytes(corrupt[33..37].try_into().unwrap()) as usize;
        corrupt[crc_offset] ^= 1;
        assert_eq!(
            prepare_png(&corrupt).unwrap().1,
            DrawioPngStatus::InvalidXml
        );
        let mut duplicate = text_xml(GRAPH);
        duplicate.splice(33..33, chunk(b"tEXt", b"mxfile\0%3Cmxfile%2F%3E"));
        assert_eq!(
            prepare_png(&duplicate).unwrap().1,
            DrawioPngStatus::InvalidXml
        );
        for kind in [b"tEXt", b"zTXt", b"iTXt"] {
            let source = with_chunk(kind, b"unrelated malformed text");
            assert_eq!(
                prepare_png(&source).unwrap(),
                (png(), DrawioPngStatus::None)
            );
        }
    }

    #[test]
    fn recognizes_ztxt_and_compressed_pages() {
        let mut compressed = ZlibEncoder::new(Vec::new(), Compression::default());
        compressed.write_all(uri(GRAPH).as_bytes()).unwrap();
        let mut data = b"mxfile\0\0".to_vec();
        data.extend(compressed.finish().unwrap());
        assert_eq!(
            prepare_png(&with_chunk(b"zTXt", &data)).unwrap().1,
            DrawioPngStatus::EmbeddedXml
        );
        let mut page = DeflateEncoder::new(Vec::new(), Compression::default());
        page.write_all(uri(GRAPH).as_bytes()).unwrap();
        let page = STANDARD.encode(page.finish().unwrap());
        assert_eq!(
            status(&format!(
                "<mxfile><diagram>{page}</diagram><diagram>{GRAPH}</diagram></mxfile>"
            )),
            DrawioPngStatus::EmbeddedXml
        );
        assert_eq!(
            status("<mxfile><diagram>broken compressed data</diagram></mxfile>"),
            DrawioPngStatus::InvalidXml
        );
    }

    #[test]
    fn rejects_entities_remote_resources_and_nested_compression() {
        assert_eq!(
            status(&format!(
                "<!DOCTYPE mxGraphModel [<!ENTITY x SYSTEM 'file:///private'>]>{GRAPH}"
            )),
            DrawioPngStatus::UnsupportedXml
        );
        assert_eq!(
            status(&GRAPH.replace("한글 https://example.invalid", "&unknown;")),
            DrawioPngStatus::InvalidXml
        );
        for attributes in [
            "style=\"image=https://example.invalid/image.png\"",
            "image=\"https://example.invalid/image.png\"",
            "value=\"&lt;img src='https://example.invalid/image.png'&gt;\"",
        ] {
            assert_eq!(
                status(&format!(
                    "<mxGraphModel><root><mxCell {attributes}/></root></mxGraphModel>"
                )),
                DrawioPngStatus::UnsupportedXml
            );
        }
        assert_eq!(status(GRAPH), DrawioPngStatus::EmbeddedXml);
        for image in ["data:image/png,AA==", "data:image/png;base64,AA=="] {
            let graph = format!(
                "<mxGraphModel><root><mxCell style=\"image={image};aspect=fixed\"/></root></mxGraphModel>"
            );
            assert_eq!(status(&graph), DrawioPngStatus::EmbeddedXml);
        }
        let mut nested = DeflateEncoder::new(Vec::new(), Compression::default());
        nested
            .write_all(format!("<mxfile><diagram>{GRAPH}</diagram></mxfile>").as_bytes())
            .unwrap();
        let nested = STANDARD.encode(nested.finish().unwrap());
        assert_eq!(
            status(&format!("<mxfile><diagram>{nested}</diagram></mxfile>")),
            DrawioPngStatus::InvalidXml
        );
    }

    #[test]
    fn bounds_cells_depth_and_raw_xml() {
        let graph = format!(
            "<mxGraphModel><root>{}</root></mxGraphModel>",
            "<mxCell/>".repeat(5000)
        );
        assert_eq!(status(&graph), DrawioPngStatus::EmbeddedXml);
        assert_eq!(
            status(&graph.replace("</root>", "<mxCell/></root>")),
            DrawioPngStatus::LimitExceeded
        );
        let deep = format!(
            "<mxGraphModel><root>{}{}</root></mxGraphModel>",
            "<object>".repeat(64),
            "</object>".repeat(64)
        );
        assert_eq!(status(&deep), DrawioPngStatus::LimitExceeded);
        let large = format!(
            "<mxGraphModel><root><mxCell value=\"{}\"/></root></mxGraphModel>",
            "x".repeat(5 * 1024 * 1024)
        );
        assert_eq!(status(&large), DrawioPngStatus::LimitExceeded);
    }

    #[test]
    fn accepts_the_exact_raw_xml_limit_and_bounds_total_expanded_pages() {
        let prefix = "<mxGraphModel><root><mxCell value=\"";
        let suffix = "\"/></root></mxGraphModel>";
        let boundary = format!(
            "{prefix}{}{suffix}",
            "x".repeat(MAX_RAW_XML - prefix.len() - suffix.len())
        );
        assert_eq!(status(&boundary), DrawioPngStatus::EmbeddedXml);
        let compress = |size: usize| {
            let mut encoder = DeflateEncoder::new(Vec::new(), Compression::fast());
            encoder.write_all(prefix.as_bytes()).unwrap();
            let block = [b'x'; 4096];
            for _ in 0..size / block.len() {
                encoder.write_all(&block).unwrap();
            }
            encoder.write_all(&block[..size % block.len()]).unwrap();
            encoder.write_all(suffix.as_bytes()).unwrap();
            STANDARD.encode(encoder.finish().unwrap())
        };
        let page = compress(12 * 1024 * 1024);
        assert_eq!(
            status(&format!("<mxfile><diagram>{page}</diagram></mxfile>")),
            DrawioPngStatus::EmbeddedXml
        );
        assert_eq!(
            status(&format!(
                "<mxfile><diagram>{page}</diagram><diagram>{page}</diagram></mxfile>"
            )),
            DrawioPngStatus::LimitExceeded
        );
        let bomb = compress(MAX_EXPANDED_XML + 1);
        assert_eq!(
            status(&format!("<mxfile><diagram>{bomb}</diagram></mxfile>")),
            DrawioPngStatus::LimitExceeded
        );
        let (preview, state) = prepare_png(&with_chunk(b"zTXt", b"mxfile\0\0bad-zlib")).unwrap();
        assert_eq!((preview, state), (png(), DrawioPngStatus::InvalidXml));
    }

    #[test]
    fn rejects_broken_chunk_framing() {
        let source = png();
        assert_eq!(
            prepare_png(&source[..source.len() - 1]),
            Err(PreviewImageError::Invalid)
        );
        let mut oversized = source;
        oversized[8..12].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(prepare_png(&oversized), Err(PreviewImageError::Invalid));
    }
}

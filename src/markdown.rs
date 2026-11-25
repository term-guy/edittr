//! A Markdown document flattened into source-anchored blocks for the preview.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use std::{iter::Peekable, ops::Range, sync::Arc};

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Style {
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub link: Option<String>,
}

#[derive(Debug, PartialEq)]
pub(crate) struct Span {
    pub text: String,
    pub style: Style,
}

#[derive(Debug, PartialEq)]
pub(crate) enum Marker {
    Bullet,
    Number(u64),
    Task(bool),
}

#[derive(Debug, PartialEq)]
pub(crate) enum Content {
    Text {
        heading: u8,
        spans: Vec<Span>,
    },
    Code {
        language: String,
        text: String,
    },
    Table {
        align: Arc<[Alignment]>,
        header: bool,
        cells: Vec<Vec<Span>>,
    },
    Rule,
}

#[derive(Debug, PartialEq)]
pub(crate) struct Block {
    pub line: usize,
    pub indent: usize,
    pub quotes: usize,
    pub marker: Option<Marker>,
    pub content: Content,
}

pub(crate) fn parse(source: &str) -> Vec<Block> {
    let events = Parser::new_ext(source, Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS).into_offset_iter();
    let mut parser = Blocks {
        events: events.peekable(),
        breaks: source.match_indices('\n').map(|(index, _)| index).collect(),
        blocks: Vec::new(),
    };
    parser.read(None, 0, 0, None);
    parser.blocks
}

struct Blocks<'a, I: Iterator<Item = (Event<'a>, Range<usize>)>> {
    events: Peekable<I>,
    breaks: Vec<usize>,
    blocks: Vec<Block>,
}

impl<'a, I: Iterator<Item = (Event<'a>, Range<usize>)>> Blocks<'a, I> {
    fn read(&mut self, end: Option<TagEnd>, indent: usize, quotes: usize, mut marker: Option<Marker>) {
        while let Some((event, range)) = self.events.next() {
            let line = self.line(range.start);
            let content = match event {
                Event::End(tag) if end == Some(tag) => break,
                Event::Start(Tag::Paragraph) => Content::Text {
                    heading: 0,
                    spans: self.inline(Some(TagEnd::Paragraph), &mut marker),
                },
                Event::Start(Tag::Heading { level, .. }) => Content::Text {
                    heading: level as u8,
                    spans: self.inline(Some(TagEnd::Heading(level)), &mut marker),
                },
                Event::Start(Tag::BlockQuote(quote)) => {
                    self.read(Some(TagEnd::BlockQuote(quote)), indent, quotes + 1, marker.take());
                    continue;
                }
                Event::Start(Tag::List(number)) => {
                    self.list(number, indent, quotes);
                    continue;
                }
                Event::Start(Tag::CodeBlock(kind)) => {
                    let language = match kind {
                        CodeBlockKind::Fenced(info) => info.split_whitespace().next().unwrap_or_default().to_owned(),
                        CodeBlockKind::Indented => String::new(),
                    };
                    let text = self.literal(TagEnd::CodeBlock);
                    Content::Code { language, text }
                }
                Event::Start(Tag::HtmlBlock) => Content::Text {
                    heading: 0,
                    spans: vec![Span {
                        text: self.literal(TagEnd::HtmlBlock),
                        style: Style::default(),
                    }],
                },
                Event::Start(Tag::Table(align)) => {
                    self.table(align.into(), indent, quotes);
                    continue;
                }
                Event::Rule => Content::Rule,
                event => {
                    // Tight lists omit the Paragraph start/end events.
                    let mut spans = Vec::new();
                    let mut styles = vec![Style::default()];
                    Self::span(event, &mut spans, &mut styles, &mut marker);
                    self.more_inline(None, &mut marker, &mut spans, &mut styles);
                    if spans.is_empty() {
                        continue;
                    }
                    Content::Text { heading: 0, spans }
                }
            };
            self.blocks.push(Block {
                line,
                indent,
                quotes,
                marker: marker.take(),
                content,
            });
        }
    }

    fn line(&self, offset: usize) -> usize {
        self.breaks.partition_point(|&n| n < offset)
    }

    fn list(&mut self, mut number: Option<u64>, indent: usize, quotes: usize) {
        while let Some((event, _)) = self.events.next() {
            match event {
                Event::Start(Tag::Item) => {
                    let marker = number.map_or(Marker::Bullet, Marker::Number);
                    self.read(Some(TagEnd::Item), indent + 1, quotes, Some(marker));
                    number = number.map(|n| n.saturating_add(1));
                }
                Event::End(TagEnd::List(_)) => break,
                _ => {}
            }
        }
    }

    fn table(&mut self, align: Arc<[Alignment]>, indent: usize, quotes: usize) {
        let mut cells = Vec::new();
        let mut line = 0;
        while let Some((event, range)) = self.events.next() {
            match event {
                Event::Start(Tag::TableHead | Tag::TableRow) => line = self.line(range.start),
                Event::Start(Tag::TableCell) => cells.push(self.inline(Some(TagEnd::TableCell), &mut None)),
                Event::End(tag @ (TagEnd::TableHead | TagEnd::TableRow)) => {
                    self.blocks.push(Block {
                        line,
                        indent,
                        quotes,
                        marker: None,
                        content: Content::Table {
                            align: align.clone(),
                            header: tag == TagEnd::TableHead,
                            cells: std::mem::take(&mut cells),
                        },
                    });
                }
                Event::End(TagEnd::Table) => break,
                _ => {}
            }
        }
    }

    fn literal(&mut self, end: TagEnd) -> String {
        let mut text = String::new();
        for (event, _) in self.events.by_ref() {
            match event {
                Event::End(tag) if tag == end => break,
                Event::Text(value) | Event::Html(value) => text.push_str(&value),
                _ => {}
            }
        }
        text
    }

    fn inline(&mut self, end: Option<TagEnd>, marker: &mut Option<Marker>) -> Vec<Span> {
        let mut spans = Vec::new();
        self.more_inline(end, marker, &mut spans, &mut vec![Style::default()]);
        spans
    }

    fn more_inline(&mut self, end: Option<TagEnd>, marker: &mut Option<Marker>, spans: &mut Vec<Span>, styles: &mut Vec<Style>) {
        while let Some((event, _)) = self.events.peek() {
            if end.is_none()
                && !matches!(
                    event,
                    Event::Text(_)
                        | Event::InlineHtml(_)
                        | Event::Code(_)
                        | Event::SoftBreak
                        | Event::HardBreak
                        | Event::TaskListMarker(_)
                        | Event::Start(Tag::Emphasis | Tag::Strong | Tag::Link { .. } | Tag::Image { .. })
                        | Event::End(TagEnd::Emphasis | TagEnd::Strong | TagEnd::Link | TagEnd::Image)
                )
            {
                break;
            }
            let (event, _) = self.events.next().unwrap();
            if let Event::End(tag) = event
                && end == Some(tag)
            {
                break;
            }
            Self::span(event, spans, styles, marker);
        }
    }

    fn span(event: Event<'_>, spans: &mut Vec<Span>, styles: &mut Vec<Style>, marker: &mut Option<Marker>) {
        let mut style = styles.last().cloned().unwrap_or_default();
        let text = match event {
            Event::Text(text) | Event::InlineHtml(text) => text.into_string(),
            Event::Code(code) => {
                style.code = true;
                code.into_string()
            }
            Event::SoftBreak => " ".into(),
            Event::HardBreak => "\n".into(),
            Event::TaskListMarker(checked) => {
                *marker = Some(Marker::Task(checked));
                return;
            }
            Event::Start(tag @ (Tag::Strong | Tag::Emphasis | Tag::Link { .. } | Tag::Image { .. })) => {
                match tag {
                    Tag::Strong => style.bold = true,
                    Tag::Emphasis | Tag::Image { .. } => style.italic = true,
                    Tag::Link { dest_url, .. } => style.link = Some(dest_url.into_string()),
                    _ => unreachable!(),
                }
                styles.push(style);
                return;
            }
            Event::End(TagEnd::Strong | TagEnd::Emphasis | TagEnd::Link | TagEnd::Image) => {
                styles.pop();
                return;
            }
            _ => return,
        };
        if let Some(last) = spans.last_mut()
            && last.style == style
        {
            last.text.push_str(&text);
        } else {
            spans.push(Span { text, style });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_inline_styles_and_source_lines() {
        let doc = parse("# Heading\n\nA **bold *nested*** `code` [link](https://example.com) ![alt](image.png).\n");
        assert!(matches!(&doc[0].content, Content::Text { heading: 1, .. }));
        assert_eq!(doc[1].line, 2);
        let Content::Text { spans, .. } = &doc[1].content else {
            panic!("expected paragraph")
        };
        assert!(
            spans
                .iter()
                .any(|span| span.text == "nested" && span.style.bold && span.style.italic)
        );
        assert!(spans.iter().any(|span| span.text == "code" && span.style.code));
        assert!(spans.iter().any(|span| span.style.link.as_deref() == Some("https://example.com")));
        assert!(spans.iter().any(|span| span.text == "alt"));
    }

    #[test]
    fn flattens_nested_lists_quotes_code_tables_and_rules() {
        let doc =
            parse("> 3. **first**\n>    - [x] task\n> 4. second\n\n```rust\nlet x = 2;\n```\n\n| A | B |\n| :- | -: |\n| a | b |\n\n---\n");
        assert_eq!(doc[0].marker, Some(Marker::Number(3)));
        assert_eq!(doc[0].quotes, 1);
        assert_eq!(doc[1].marker, Some(Marker::Task(true)));
        assert_eq!(doc[1].indent, 2);
        assert_eq!(doc[2].marker, Some(Marker::Number(4)));
        assert!(matches!(&doc[3].content, Content::Code { language, text } if language == "rust" && text == "let x = 2;\n"));
        assert!(
            matches!(&doc[4].content, Content::Table { header: true, align, cells } if align.as_ref() == [Alignment::Left, Alignment::Right] && cells.len() == 2)
        );
        assert!(matches!(&doc[5].content, Content::Table { header: false, .. }));
        assert_eq!(doc[6].content, Content::Rule);
    }
}

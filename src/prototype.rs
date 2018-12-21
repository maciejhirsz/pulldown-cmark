// Copyright 2017 Google Inc. All rights reserved.
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
// THE SOFTWARE.

//! Prototype of tree-based two pass parser.

use std::borrow::Cow;
use std::borrow::Cow::Borrowed;
use std::collections::HashMap;

use linklabel::LinkLabel;
use parse::{Event, Tag, Options};
use scanners::*;
use tree::{NIL, Node, Tree};

#[derive(Debug)]
struct Item {
    start: usize,
    end: usize,
    body: ItemBody,
}

#[derive(Debug, PartialEq)]
enum ItemBody {
    Paragraph,
    Text,
    SoftBreak,
    HardBreak,

    // These are possible inline items, need to be resolved in second pass.
    Inline(usize, bool, bool), // Perhaps this should be MaybeEmphasis?
    MaybeCode(usize),
    MaybeHtml,
    MaybeLinkOpen,
    MaybeLinkClose,
    Backslash,

    // These are inline items after resolution.
    Emphasis,
    Strong,
    Code,
    InlineHtml,
    // Link params: destination, title.
    // TODO: get lifetime in type so this can be a cow
    Link(String, String),

    Rule,
    Header(i32), // header level
    FencedCodeBlock(String), // info string (maybe cow?)
    IndentCodeBlock(usize), // last non-blank child
    SynthesizeNewLine,  // TODO: subsume under SynthesizeText, or delete
    HtmlBlock(Option<&'static str>), // end tag, or none for type 6
    Html,
    BlockQuote,
    List(bool, u8, Option<usize>), // is_tight, list character, list start index
    ListItem(usize), // indent level
    SynthesizeText(Cow<'static, str>),
    BlankLine,
}

/// State for the first parsing pass.
///
/// The first pass resolves all block structure, generating an AST. Within a block, items
/// are in a linear chain with potential inline markup identified.
struct FirstPass<'a> {
    text: &'a str,
    tree: Tree<Item>,
    last_line_blank: bool,
    links: HashMap<LinkLabel<'a>, (Cow<'a, str>, Cow<'a, str>)>,
}

impl<'a> FirstPass<'a> {
    fn new(text: &str) -> FirstPass {
        let tree = Tree::new();
        let last_line_blank = false;
        let links = HashMap::new();
        FirstPass { text, tree, last_line_blank, links }
    }

    fn run(mut self) -> Tree<Item> {
        let mut ix = 0;
        while ix < self.text.len() {
            ix = self.parse_block(ix);
        }
        for _ in 0..self.tree.spine.len() {
            self.pop(ix);
        }
        //dump_tree(&self.tree.nodes, 0, 0);
        self.tree
    }

    /// Returns offset after block.
    fn parse_block(&mut self, start_ix: usize) -> usize {
        let mut line_start = LineStart::new(&self.text[start_ix..]);

        let i = self.scan_containers(&mut line_start);
        for _ in i..self.tree.spine.len() {
            self.pop(start_ix);
        }

        // Process new containers
        loop {
            let container_start = start_ix + line_start.bytes_scanned();
            if line_start.scan_blockquote_marker() {
                self.finish_list(start_ix);
                self.tree.append(Item {
                    start: container_start,
                    end: 0,  // will get set later
                    body: ItemBody::BlockQuote,
                });
                self.tree.push();
            } else if let Some((ch, index, indent)) = line_start.scan_list_marker() {
                let opt_index = if ch == b'.' || ch == b')' { Some(index) } else { None };
                self.continue_list(container_start, ch, opt_index);
                self.tree.append(Item {
                    start: container_start,
                    end: 0,  // will get set later
                    body: ItemBody::ListItem(indent),
                });
                self.tree.push();
            } else {
                break;
            }
        }

        let ix = start_ix + line_start.bytes_scanned();
        if let Some(n) = scan_blank_line(&self.text[ix..]) {
            self.last_line_blank = true;
            return ix + n;
        }

        self.finish_list(start_ix);

        // Save `remaining_space` here to avoid needing to backtrack `line_start` for HTML blocks
        let remaining_space = line_start.remaining_space();
        let indent = line_start.scan_space_upto(4);
        if indent == 4 {
            let ix = start_ix + line_start.bytes_scanned();
            let remaining_space = line_start.remaining_space();
            return self.parse_indented_code_block(ix, remaining_space);
        }


        // HTML Blocks

        // Start scanning at the first nonspace character, but don't advance `ix` yet because any
        // spaces present before the HTML block begins should be preserved.
        let nonspace_ix = start_ix + line_start.bytes_scanned();

        // Types 1-5 are all detected by one function and all end with the same
        // pattern
        if let Some(html_end_tag) = get_html_end_tag(&self.text[nonspace_ix..]) {
            return self.parse_html_block_type_1_to_5(ix, html_end_tag, remaining_space);
        }

        // Detect type 6
        let possible_tag = scan_html_block_tag(&self.text[nonspace_ix..]).1;
        if is_html_tag(possible_tag) {
            return self.parse_html_block_type_6_or_7(ix, remaining_space);
        }

        // Detect type 7
        if let Some(_html_bytes) = scan_html_type_7(&self.text[nonspace_ix..]) {
            return self.parse_html_block_type_6_or_7(ix, remaining_space);
        }

        // Advance `ix` after HTML blocks have been scanned
        let ix = start_ix + line_start.bytes_scanned();

        let n = scan_hrule(&self.text[ix..]);
        if n > 0 {
            return self.parse_hrule(n, ix);
        }

        if let Some((atx_size, atx_level)) = scan_atx_heading(&self.text[ix..]) {
            return self.parse_atx_heading(ix, atx_level, atx_size);
        }

        let (n, fence_ch) = scan_code_fence(&self.text[ix..]);
        if n > 0 {
            return self.parse_fenced_code_block(ix, indent, fence_ch, n);
        }
        self.parse_paragraph(ix)
    }

    /// Return offset of line start after paragraph.
    fn parse_paragraph(&mut self, start_ix: usize) -> usize {
        self.tree.append(Item {
            start: start_ix,
            end: 0,  // will get set later
            body: ItemBody::Paragraph,
        });
        let node = self.tree.cur;
        self.tree.push();

        let mut ix = start_ix;
        loop {
            let (next_ix, brk) = parse_line(&mut self.tree, &self.text, ix);
            ix = next_ix;

            let mut line_start = LineStart::new(&self.text[ix..]);
            let n_containers = self.scan_containers(&mut line_start);
            if !line_start.scan_space(4) {
                let ix_new = ix + line_start.bytes_scanned();
                if n_containers == self.tree.spine.len() {
                    if let Some((n, level)) = scan_setext_heading(&self.text[ix_new..]) {
                        self.tree.nodes[node].item.body = ItemBody::Header(level);
                        if let Some(Item { start, end: _, body: ItemBody::HardBreak }) = brk {
                            if self.text.as_bytes()[start] == b'\\' {
                                self.tree.append_text(start, start + 1);
                            }
                        }
                        ix = ix_new + n;
                        break;
                    }
                }
                if scan_paragraph_interrupt(&self.text[ix_new..]) {
                    break;
                }
            }
            line_start.scan_all_space();
            ix = next_ix + line_start.bytes_scanned();
            if let Some(item) = brk {
                self.tree.append(item);
            }
        }

        self.tree.pop();
        self.tree.nodes[self.tree.cur].item.end = ix;
        ix
    }

    /// When start_ix is at the beginning of an HTML block of type 1 to 5,
    /// this will find the end of the block, adding the block itself to the
    /// tree and also keeping track of the lines of HTML within the block.
    ///
    /// The html_end_tag is the tag that must be found on a line to end the block.
    fn parse_html_block_type_1_to_5(&mut self, start_ix: usize, html_end_tag: &'static str,
            mut remaining_space: usize) -> usize
    {
        self.tree.append(Item {
            start: start_ix,
            end: 0, // set later
            body: ItemBody::HtmlBlock(Some(html_end_tag)),
        });
        self.tree.push();

        let mut ix = start_ix;
        let end_ix;
        loop {
            let line_start_ix = ix;
            ix += scan_nextline(&self.text[ix..]);
            self.append_html_line(remaining_space, line_start_ix, ix);

            let mut line_start = LineStart::new(&self.text[ix..]);
            let n_containers = self.scan_containers(&mut line_start);
            if n_containers < self.tree.spine.len() {
                end_ix = ix;
                break;
            }

            if (&self.text[line_start_ix..ix]).contains(html_end_tag) {
                end_ix = ix;
                break;
            }

            let next_line_ix = ix + line_start.bytes_scanned();
            if next_line_ix == self.text.len() {
                end_ix = next_line_ix;
                break;
            }
            ix = next_line_ix;
            remaining_space = line_start.remaining_space();
        }
        &self.pop(end_ix);
        ix
    }

    /// When start_ix is at the beginning of an HTML block of type 6 or 7,
    /// this will consume lines until there is a blank line and keep track of
    /// the HTML within the block.
    fn parse_html_block_type_6_or_7(&mut self, start_ix: usize, mut remaining_space: usize)
        -> usize
    {
        self.tree.append(Item {
            start: start_ix,
            end: 0, // set later
            body: ItemBody::HtmlBlock(None)
        });
        self.tree.push();

        let mut ix = start_ix;
        let end_ix;
        loop {
            let line_start_ix = ix;
            ix += scan_nextline(&self.text[ix..]);
            self.append_html_line(remaining_space, line_start_ix, ix);

            let mut line_start = LineStart::new(&self.text[ix..]);
            let n_containers = self.scan_containers(&mut line_start);
            if n_containers < self.tree.spine.len() || line_start.is_at_eol()
            {
                end_ix = ix;
                break;
            }

            let next_line_ix = ix + line_start.bytes_scanned();
            if next_line_ix == self.text.len()
                || scan_blank_line(&self.text[next_line_ix..]).is_some()
            {
                end_ix = next_line_ix;
                break;
            }
            ix = next_line_ix;
            remaining_space = line_start.remaining_space();
        }
        self.pop(end_ix);
        ix
    }

    fn parse_indented_code_block(&mut self, start_ix: usize, mut remaining_space: usize)
        -> usize
    {
        self.tree.append(Item {
            start: start_ix,
            end: 0,  // will get set later
            body: ItemBody::IndentCodeBlock(0), // TODO: probably remove arg
        });
        self.tree.push();
        let mut last_nonblank_child = NIL;
        let mut end_ix = 0;
        let mut last_line_blank = false;

        let mut ix = start_ix;
        loop {
            let line_start_ix = ix;
            ix += scan_nextline(&self.text[ix..]);
            self.append_code_text(remaining_space, line_start_ix, ix);
            // TODO(spec clarification): should we synthesize newline at EOF?

            if !last_line_blank {
                last_nonblank_child = self.tree.cur;
                end_ix = ix;
            }

            let mut line_start = LineStart::new(&self.text[ix..]);
            let n_containers = self.scan_containers(&mut line_start);
            if n_containers < self.tree.spine.len()
                || !(line_start.scan_space(4) || line_start.is_at_eol())
            {
                break;
            }
            let next_line_ix = ix + line_start.bytes_scanned();
            if next_line_ix == self.text.len() {
                break;
            }
            ix = next_line_ix;
            remaining_space = line_start.remaining_space();
            last_line_blank = scan_blank_line(&self.text[ix..]).is_some();
        }

        // Trim trailing blank lines.
        self.tree.nodes[last_nonblank_child].next = NIL;
        self.pop(end_ix);
        ix
    }

    fn parse_fenced_code_block(&mut self, start_ix: usize, indent: usize,
        fence_ch: u8, n_fence_char: usize) -> usize
    {
        let mut info_start = start_ix + n_fence_char;
        info_start += scan_whitespace_no_nl(&self.text[info_start..]);
        let mut info_end = info_start + scan_nextline(&self.text[info_start..]);
        while info_end > info_start && is_ascii_whitespace(self.text.as_bytes()[info_end - 1]) {
            info_end -= 1;
        }
        let info_string = self.text[info_start..info_end].to_string();
        self.tree.append(Item {
            start: start_ix,
            end: 0,  // will get set later
            body: ItemBody::FencedCodeBlock(info_string),
        });
        self.tree.push();
        let mut ix = start_ix + scan_nextline(&self.text[start_ix..]);
        loop {
            let mut line_start = LineStart::new(&self.text[ix..]);
            let n_containers = self.scan_containers(&mut line_start);
            if n_containers < self.tree.spine.len() {
                break;
            }
            line_start.scan_space(indent);
            let mut close_line_start = line_start.clone();
            if !close_line_start.scan_space(4) {
                let close_ix = ix + close_line_start.bytes_scanned();
                if let Some(n) =
                    scan_closing_code_fence(&self.text[close_ix..], fence_ch, n_fence_char)
                {
                    ix = close_ix + n;
                    break;
                }
            }
            let remaining_space = line_start.remaining_space();
            ix += line_start.bytes_scanned();
            let next_ix = ix + scan_nextline(&self.text[ix..]);
            self.append_code_text(remaining_space, ix, next_ix);
            ix = next_ix;
        }
        self.pop(ix);
        ix
    }

    fn append_code_text(&mut self, remaining_space: usize, start: usize, end: usize) {
        if remaining_space > 0 {
            self.tree.append(Item {
                start: start,
                end: start,
                body: ItemBody::SynthesizeText(Borrowed(&"   "[..remaining_space])),
            });
        }
        if self.text.as_bytes()[end - 2] == b'\r' {
            // Normalize CRLF to LF
            self.tree.append_text(start, end - 2);
            self.tree.append_text(end - 1, end);
        } else {
            self.tree.append_text(start, end);
        }
    }


    /// Appends a line of HTML to the tree.
    fn append_html_line(&mut self, remaining_space: usize, start: usize, end: usize) {
        if remaining_space > 0 {
            self.tree.append(Item {
                start: start,
                end: start,
                // TODO: maybe this should synthesize to html rather than text?
                body: ItemBody::SynthesizeText(Borrowed(&"   "[..remaining_space])),
            });
        }
        if self.text.as_bytes()[end - 2] == b'\r' {
            // Normalize CRLF to LF
            self.tree.append(Item {
                start: start,
                end: end - 2,
                body: ItemBody::Html,
            });
            self.tree.append(Item {
                start: end - 1,
                end: end,
                body: ItemBody::Html,
            });
        } else {
            self.tree.append(Item {
                start: start,
                end: end,
                body: ItemBody::Html,
            });
        }
    }

    /// Returns number of containers scanned.
    fn scan_containers(&self, line_start: &mut LineStart) -> usize {
        let mut i = 0;
        for &node_ix in &self.tree.spine {
            match self.tree.nodes[node_ix].item.body {
                ItemBody::BlockQuote => {
                    if !line_start.scan_blockquote_marker() {
                        break;
                    }
                }
                ItemBody::List(_, _, _) => (),
                ItemBody::ListItem(indent) => {
                    if !(line_start.scan_space(indent) || line_start.is_at_eol()) {
                        break;
                    }
                }
                ItemBody::Paragraph => (),
                ItemBody::IndentCodeBlock(_) => (),
                ItemBody::FencedCodeBlock(_) => (),
                ItemBody::HtmlBlock(_) => (),
                _ => panic!("unexpected node in tree"),
            }
            i += 1;
        }
        i
    }

    /// Pop a container, setting its end.
    fn pop(&mut self, ix: usize) {
        self.tree.pop();
        self.tree.nodes[self.tree.cur].item.end = ix;
        if let ItemBody::List(true, _, _) = self.tree.nodes[self.tree.cur].item.body {
            surgerize_tight_list(&mut self.tree);
        }
    }

    /// Close a list if it's open. Also set loose if last line was blank.
    fn finish_list(&mut self, ix: usize) {
        if let Some(node_ix) = self.tree.peek_up() {
            if let ItemBody::List(_, _, _) = self.tree.nodes[node_ix].item.body {
                self.pop(ix);
            }
        }
        if self.last_line_blank {
            if let Some(node_ix) = self.tree.peek_grandparent() {
                if let ItemBody::List(ref mut is_tight, _, _) =
                    self.tree.nodes[node_ix].item.body
                {
                    *is_tight = false;
                }
            }
            self.last_line_blank = false;
        }
    }

    /// Continue an existing list or start a new one if there's not an open
    /// list that matches.
    fn continue_list(&mut self, start: usize, ch: u8, index: Option<usize>) {
        if let Some(node_ix) = self.tree.peek_up() {
            if let ItemBody::List(ref mut is_tight, existing_ch, _) =
                self.tree.nodes[node_ix].item.body
            {
                if existing_ch == ch {
                    if self.last_line_blank {
                        *is_tight = false;
                        self.last_line_blank = false;
                    }
                    return;
                }
            }
            // TODO: this is not the best choice for end; maybe get end from last list item.
            self.finish_list(start);
        }
        self.tree.append(Item {
            start: start,
            end: 0,  // will get set later
            body: ItemBody::List(true, ch, index),
        });
        self.tree.push();
        self.last_line_blank = false;
    }

    /// Parse a thematic break.
    ///
    /// Returns index of start of next line.
    fn parse_hrule(&mut self, hrule_size: usize, ix: usize) -> usize {
        self.tree.append(Item {
            start: ix,
            end: ix + hrule_size,
            body: ItemBody::Rule,
        });
        ix + hrule_size
    }

    /// Parse an ATX heading.
    ///
    /// Returns index of start of next line.
    fn parse_atx_heading(&mut self, mut ix: usize, atx_level: i32, atx_size: usize) -> usize {
        self.tree.append(Item {
            start: ix,
            end: 0, // set later
            body: ItemBody::Header(atx_level),
        });
        ix += atx_size;
        // next char is space or scan_eol
        // (guaranteed by scan_atx_heading)
        let b = self.text.as_bytes()[ix];
        if b == b'\n' || b == b'\r' {
            ix += scan_eol(&self.text[ix..]).0;
            return ix;
        }
        // skip leading spaces
        let skip_spaces = scan_whitespace_no_nl(&self.text[ix..]);
        ix += skip_spaces;

        // now handle the header text
        let header_start = ix;
        let header_node_idx = self.tree.cur; // so that we can set the endpoint later
        self.tree.push();
        ix = parse_line(&mut self.tree, &self.text, ix).0;
        self.tree.nodes[header_node_idx].item.end = ix;

        // remove trailing matter from header text
        // TODO: probably better to find limit before parsing; this makes assumptions
        // about the way the line is parsed.
        let header_text = &self.text[header_start..];
        let mut limit = ix - header_start;
        if limit > 0 && header_text.as_bytes()[limit-1] == b'\n' {
            limit -= 1;
        }
        if limit > 0 && header_text.as_bytes()[limit-1] == b'\r' {
            limit -= 1;
        }
        while limit > 0 && header_text.as_bytes()[limit-1] == b' ' {
            limit -= 1;
        }
        let mut closer = limit;
        while closer > 0 && header_text.as_bytes()[closer-1] == b'#' {
            closer -= 1;
        }
        if closer > 0 && header_text.as_bytes()[closer-1] == b' ' {
            limit = closer;
            while limit > 0 && header_text.as_bytes()[limit-1] == b' ' {
                limit -= 1;
            }
        } else if closer == 0 { limit = closer; }
        if self.tree.cur != NIL {
            self.tree.nodes[self.tree.cur].item.end = limit + header_start;
        }

        self.tree.pop();
        ix
    }
}

impl Tree<Item> {
    fn append_text(&mut self, start: usize, end: usize) {
        if end > start {
            self.append(Item {
                start: start,
                end: end,
                body: ItemBody::Text,
            });
        }
    }

    fn append_html_line(&mut self, start: usize, end: usize) {
        if end >= start {
            self.append(Item {
                start: start,
                end: end,
                body: ItemBody::Html,
            });
            self.append(Item {
                start: end,
                end: end,
                body: ItemBody::SynthesizeNewLine,
            });
        }
    }

    fn append_newline(&mut self, ix: usize) {
        self.append(Item {
            start: ix,
            end: ix,
            body: ItemBody::SynthesizeNewLine,
        });
    }
}

#[allow(dead_code)]
fn dump_tree(nodes: &Vec<Node<Item>>, mut ix: usize, level: usize) {
    while ix != NIL {
        let node = &nodes[ix];
        for _ in 0..level {
            print!("  ");
        }
        println!("{}: {:?} {} {}", ix, node.item.body, node.item.start, node.item.end);
        dump_tree(nodes, node.child, level + 1);
        ix = node.next;
    }
}

/// Parse a line of input, appending text and items to tree.
///
/// Returns: index after line and an item representing the break.
fn parse_line(tree: &mut Tree<Item>, s: &str, mut ix: usize) -> (usize, Option<Item>) {
    let bytes = s.as_bytes();
    let start = ix;
    let mut begin_text = start;
    while ix < s.len() {
        match bytes[ix] {
            b'\n' | b'\r' => {
                let mut i = ix;
                if ix >= begin_text + 1 && bytes[ix - 1] == b'\\' {
                    i -= 1;
                    tree.append_text(begin_text, i);
                    ix += scan_eol(&s[ix..]).0;
                    return (ix, Some(Item {
                        start: i,
                        end: ix,
                        body: ItemBody::HardBreak,
                    }));
                } else if ix >= begin_text + 2
                    && is_ascii_whitespace_no_nl(bytes[ix - 1])
                    && is_ascii_whitespace_no_nl(bytes[ix - 2]) {
                    i -= 2;
                    while i > 0 && is_ascii_whitespace_no_nl(bytes[i - 1]) {
                        i -= 1;
                    }
                    tree.append_text(begin_text, i);
                    ix += scan_eol(&s[ix..]).0;
                    return (ix, Some(Item {
                        start: i,
                        end: ix,
                        body: ItemBody::HardBreak,
                    }));
                }
                tree.append_text(begin_text, ix);
                ix += scan_eol(&s[ix..]).0;
                return (ix, Some(Item {
                    start: i,
                    end: ix,
                    body: ItemBody::SoftBreak,
                }));
            }
            b'\\' if ix + 1 < s.len() && is_ascii_punctuation(bytes[ix + 1]) => {
                tree.append_text(begin_text, ix);
                tree.append(Item {
                    start: ix,
                    end: ix + 1,
                    body: ItemBody::Backslash,
                });
                begin_text = ix + 1;
                ix += 2;
            }
            c @ b'*' | c @b'_' => {
                tree.append_text(begin_text, ix);
                let count = 1 + scan_ch_repeat(&s[ix+1..], c);
                let can_open = ix + count < s.len() && !is_ascii_whitespace(bytes[ix + count]);
                let can_close = ix > start && !is_ascii_whitespace(bytes[ix - 1]);
                // TODO: can skip if neither can_open nor can_close
                for i in 0..count {
                    tree.append(Item {
                        start: ix + i,
                        end: ix + i + 1,
                        body: ItemBody::Inline(count - i, can_open, can_close),
                    });
                }
                ix += count;
                begin_text = ix;
            }
            b'`' => {
                tree.append_text(begin_text, ix);
                let count = 1 + scan_ch_repeat(&s[ix+1..], b'`');
                tree.append(Item {
                    start: ix,
                    end: ix + count,
                    body: ItemBody::MaybeCode(count),
                });
                ix += count;
                begin_text = ix;
            }
            b'<' => {
                // Note: could detect some non-HTML cases and early escape here, but not
                // clear that's a win.
                tree.append_text(begin_text, ix);
                tree.append(Item {
                    start: ix,
                    end: ix + 1,
                    body: ItemBody::MaybeHtml,
                });
                ix += 1;
                begin_text = ix;
            }
            b'[' => {
                tree.append_text(begin_text, ix);
                tree.append(Item {
                    start: ix,
                    end: ix + 1,
                    body: ItemBody::MaybeLinkOpen,
                });
                ix += 1;
                begin_text = ix;
            }
            b']' => {
                tree.append_text(begin_text, ix);
                tree.append(Item {
                    start: ix,
                    end: ix + 1,
                    body: ItemBody::MaybeLinkClose,
                });
                ix += 1;
                begin_text = ix;
            }
            _ => ix += 1,
        }
    }
    // need to close text at eof
    tree.append_text(begin_text, ix);
    (ix, None)
}

// ix is at the beginning of the code line text
// returns the index of the start of the next line
fn parse_indented_code_line(tree: &mut Tree<Item>, s: &str, mut ix: usize) -> usize {
    
    let codeline_end_offset = scan_line_ending(&s[ix..]);
    tree.append_text(ix, codeline_end_offset + ix);
    tree.append_newline(codeline_end_offset);

    // record the last nonblank child so that we can remove
    // trailing blanklines during tree parsing
    if let None = scan_blank_line(&s[ix..]) {
        let parent_icb = tree.peek_up().unwrap(); // this line must have an icb parent
        if let ItemBody::IndentCodeBlock(ref mut last_nonblank_child) = tree.nodes[parent_icb].item.body {
            *last_nonblank_child = tree.cur;
        }
    }

    ix += codeline_end_offset;
    ix += scan_eol(&s[ix..]).0;
    ix
}

// Returns index of start of next line.
fn parse_hrule(tree: &mut Tree<Item>, hrule_size: usize, mut ix: usize) -> usize {
    tree.append(Item {
        start: ix,
        end: ix + hrule_size,
        body: ItemBody::Rule,
    });
    ix += hrule_size;
    ix
}

fn parse_html_line_type_1_to_5(tree : &mut Tree<Item>, s : &str, mut ix : usize, html_end_tag: &'static str) -> usize {
    let nextline_offset = scan_nextline(&s[ix..]);
    let htmlline_end_offset = scan_line_ending(&s[ix..]);
    tree.append_html_line(ix, ix+htmlline_end_offset);
    if (&s[ix..ix+htmlline_end_offset]).contains(html_end_tag) {
        tree.pop(); // to HTML Block
    }
    ix += nextline_offset;
    ix
}

fn parse_html_line_type_6or7(tree : &mut Tree<Item>, s : &str, mut ix : usize) -> usize {
    let nextline_offset = scan_nextline(&s[ix..]);
    let htmlline_end_offset = scan_line_ending(&s[ix..]);
    tree.append_html_line(ix, ix+htmlline_end_offset);
    if let Some(_) = scan_blank_line(&s[ix+nextline_offset..]) {
        tree.pop();
    }
    ix += nextline_offset;
    ix
}

fn scan_paragraph_interrupt(s: &str) -> bool {
    scan_eol(s).1 ||
    scan_hrule(s) > 0 ||
    scan_atx_heading(s).is_some() ||
    scan_code_fence(s).0 > 0 ||
    get_html_end_tag(s).is_some() ||
    scan_blockquote_start(s) > 0 ||
    scan_listitem(s).0 > 0 ||  // TODO: be stricter with ordered lists
    is_html_tag(scan_html_block_tag(s).1)
}

#[allow(unused)]
fn parse_paragraph_old(mut tree : &mut Tree<Item>, s : &str, mut ix : usize) -> usize {
    tree.append(Item {
        start: ix,
        end: 0,  // will get set later
        body: ItemBody::Paragraph,
    });
    let cur = tree.cur;
    tree.push();
    let mut last_soft_break = None;
    while ix < s.len() {
        let line_start = ix;

        let container_scan = scan_containers_old(&tree, &s[ix..]);
        ix += container_scan.0;

        let (leading_bytes, leading_spaces) = scan_leading_space(&s[ix..], 0);
        ix += leading_bytes;

       
        let (setext_bytes, setext_level) = scan_setext_header(&s[ix..]);
        // setext headers can't be lazy paragraph continuations
        if !container_scan.1 {
            if setext_bytes > 0 && leading_spaces < 4 {
                break; 
            }
        }
        // setext headers can interrupt paragraphs
        // but can't be preceded by an empty line. 
        if setext_bytes > 0 && leading_spaces < 4 && tree.cur != NIL {
            ix += setext_bytes;
            tree.nodes[cur].item.body = ItemBody::Header(setext_level);
            break;
        }

        if leading_spaces < 4 && scan_paragraph_interrupt(&s[ix..]) {
            ix = line_start; 
            break; }

        if let Some(pos) = last_soft_break {
            tree.append(Item {
                start: pos,
                end: pos + 1,  // TODO: handle \r\n
                body: ItemBody::SoftBreak,
            });
        }
        let n = parse_line(&mut tree, s, ix).0;
        ix += n;
        if let (n, true) = scan_eol(&s[ix..]) {
            last_soft_break = Some(ix);
            ix += n;  // skip newline
        }
    }
    tree.pop();
    tree.nodes[cur].item.end = ix;
    ix
}

// Scan markers and indentation for current container stack
// Scans to the first character after the container marks
// Return: bytes scanned, and whether containers were closed
fn scan_containers_old(tree: &Tree<Item>, text: &str) -> (usize, bool) {
    let mut i = 0;
    for &vertebra in &(tree.spine) {
        let (space_bytes, num_spaces) = scan_leading_space(&text[i..],0);
        
        match tree.nodes[vertebra].item.body {
            ItemBody::BlockQuote => {
                i += space_bytes;
                if num_spaces >= 4 { return (0, false); }
                let n = scan_blockquote_start(&text[i..]);
                if n > 0 {
                    i += n
                } else {
                    return (i, false);
                }
            },
            ItemBody::ListItem(indent) => {
                if !(num_spaces >= indent || scan_eol(&text[i..]).1) {
                    return (i, false);
                } else if scan_eol(&text[i..]).1 {
                    if let ItemBody::BlankLine = tree.nodes[tree.cur].item.body {
                        if tree.nodes[vertebra].child == tree.cur {
                            return (i, false);
                        }
                    }
                    return (i, true);
                }
                i += indent;

            },
            ItemBody::IndentCodeBlock(_) => {
                if let Some(codeline_start_offset) = scan_code_line(&text[i..]) {
                    i += codeline_start_offset;
                    return (i, true);
                } else {
                    return (0, false);
                }
            }
            ItemBody::List(_, _, _) => {
                // hrule interrupts list
                let hrule_size = scan_hrule(&text[i..]);
                if hrule_size > 0 {
                    return (0, false);
                }
            }
            _ => (),
        }
    }
    return (i, true);
}

// Used on a new line, after scan_containers_old
// scans to first character after new container markers
fn parse_new_containers(tree: &mut Tree<Item>, s: &str, mut ix: usize) -> usize {
    if ix >= s.len() { return ix; }
    // check if parent is a leaf block, which makes new containers illegal
    if let Some(parent) = tree.peek_up() {
        if let ItemBody::FencedCodeBlock(_) = tree.nodes[parent].item.body {
            return ix;
        }
        if let ItemBody::IndentCodeBlock(_) = tree.nodes[parent].item.body {
            return ix;
        }
        if let ItemBody::HtmlBlock(_) = tree.nodes[parent].item.body {
            return ix;
        }
    }
    let begin = ix;
    let leading_bytes = scan_leading_space(s, ix).0;
    loop {
        let (leading_bytes, leading_spaces) = scan_leading_space(s, ix);
        if leading_spaces >= 4 { break; }
        ix += leading_bytes;
        
        let blockquote_bytes = scan_blockquote_start(&s[ix..]);
        if blockquote_bytes > 0 {
            tree.append(Item {
                start: ix,
                end: ix, // TODO: set this correctly
                body: ItemBody::BlockQuote,
            });
            tree.push();
            ix += blockquote_bytes;
            continue;
        }

        let (listitem_bytes, listitem_delimiter, listitem_start_index, listitem_indent) = scan_listitem(&s[ix..]);
        if listitem_bytes > 0 {
            // thematic breaks take precedence over listitems
            if scan_hrule(&s[ix..]) > 0 { break; }

            let listitem_start;
            // handle ordered lists
            if listitem_delimiter == b'.' || listitem_delimiter == b')' {
                listitem_start = Some(listitem_start_index);
            } else {
                listitem_start = None;
            }

            let mut need_push = true; // Are we starting a new list?
            if let Some(parent) = tree.peek_up() {
                match tree.nodes[parent].item.body {
                    ItemBody::List(_, delim, _) if delim == listitem_delimiter => {
                        need_push = false;
                    },
                    ItemBody::List(_, _, _) => {
                        // A different delimiter indicates a new list
                        tree.pop();
                    },
                    _ => {},
                }
            }
            if need_push {
                tree.append(Item {
                    start: ix,
                    end: ix, // TODO: set this correctly
                    body: ItemBody::List(false /* */, listitem_delimiter, listitem_start),
                });
                tree.push();
            }

            tree.append(Item {
                start: ix,
                end: ix, // TODO: set this correctly
                body: ItemBody::ListItem(listitem_indent + leading_spaces),
            });
            tree.push();
            ix += listitem_bytes;
            continue;
        }
        break;
    }

    // If we are at a ListItem node, we didn't see a new ListItem,
    // so it's time to close the list.
    if tree.cur != NIL {
        if let ItemBody::ListItem(_) = tree.nodes[tree.cur].item.body {
            tree.pop();
        }
    }

    if ix > leading_bytes + begin {
        return ix;
    } else {
        return begin;
    }
}

// Used on a new line, after scan_containers_old and scan_new_containers.
// Mutates tree as needed, and returns the start of the next line.
fn parse_blocks(mut tree: &mut Tree<Item>, s: &str, mut ix: usize) -> usize {
    if ix >= s.len() { return ix; }

    if let Some(parent) = tree.peek_up() {
        /*
        if let ItemBody::FencedCodeBlock(num_fence_char, fence_char, indentation, _) = tree.nodes[parent].item.body {
            return parse_fenced_code_line(&mut tree, s, ix, num_fence_char, fence_char, indentation);
        }
        */
        if let ItemBody::IndentCodeBlock(_) = tree.nodes[parent].item.body {
            return parse_indented_code_line(&mut tree, s, ix);
        }
        if let ItemBody::HtmlBlock(Some(html_end_tag)) = tree.nodes[parent].item.body {
            return parse_html_line_type_1_to_5(&mut tree, s, ix, html_end_tag);
        }
        if let ItemBody::HtmlBlock(None) = tree.nodes[parent].item.body {
            return parse_html_line_type_6or7(&mut tree, s, ix);
        }
    }

    if let Some(blankline_size) = scan_blank_line(&s[ix..]) {
        tree.append(Item {
            start: ix,
            end: ix + blankline_size,
            body: ItemBody::BlankLine,
        });

        ix += blankline_size;
        return ix;
    }

    let (leading_bytes, _leading_spaces) = scan_leading_space(&s[ix..], 0);
    
    if let Some(codeline_start_offset) = scan_code_line(&s[ix..]) {
        tree.append(Item {
            start: ix,
            end: 0, // set later
            body: ItemBody::IndentCodeBlock(NIL)
        });
        tree.push();
        ix += codeline_start_offset;
        return parse_indented_code_line(&mut tree, s, ix);
    }

    // leading spaces are preserved in html blocks
    if let Some(html_end_tag) = get_html_end_tag(&s[ix+leading_bytes..]) {
        tree.append(Item {
            start: ix,
            end: 0, // set later
            body: ItemBody::HtmlBlock(Some(html_end_tag)),
        });
        tree.push();
        return parse_html_line_type_1_to_5(&mut tree, s, ix, html_end_tag);
    }

    let possible_tag = scan_html_block_tag(&s[ix+leading_bytes..]).1;
    if is_html_tag(possible_tag) {
        tree.append(Item {
            start: ix,
            end: 0, // set later
            body: ItemBody::HtmlBlock(None)
        });
        tree.push();
        return parse_html_line_type_6or7(&mut tree, s, ix);
    }

    if let Some(html_bytes) = scan_html_type_7(&s[ix+leading_bytes..]) {
        tree.append(Item {
            start: ix,
            end: 0, // set later
            body: ItemBody::HtmlBlock(None)
        });
        tree.push();
        tree.append_html_line(ix, ix+html_bytes);
        ix += html_bytes;
        let nextline_offset = scan_nextline(&s[ix..]);
        return ix + nextline_offset;
    }



    ix += leading_bytes;

    let (_atx_size, atx_level) = scan_atx_header(&s[ix..]);
    if atx_level > 0 {
        unimplemented!();
        //return parse_atx_header(&mut tree, s, ix, atx_level, atx_size);
    }

    let hrule_size = scan_hrule(&s[ix..]);
    if hrule_size > 0 {
        return parse_hrule(&mut tree, hrule_size, ix);
    }

    let (num_code_fence_chars, _code_fence_char) = scan_code_fence(&s[ix..]);
    if num_code_fence_chars > 0 {
        let nextline_offset = scan_nextline(&s[ix..]);
        let info_string = unescape(s[ix+num_code_fence_chars..ix+nextline_offset].trim()).to_string();
        tree.append(Item {
            start: ix,
            end: 0, // set later
            body: ItemBody::FencedCodeBlock(info_string),
        });
        
        ix += scan_nextline(&s[ix..]);

        tree.push();
        return ix;
    }

    unimplemented!();
    //return parse_paragraph(&mut tree, s, ix);
    // }
}

#[allow(unused)]
// Root is node 0
fn first_pass_old(s: &str) -> Tree<Item> {
    let mut tree = Tree::new();
    let mut ix = 0;
    while ix < s.len() {
        // start of a new line
        let (container_offset, are_containers_closed) = scan_containers_old(&mut tree, &s[ix..]);
        if !are_containers_closed {
            tree.pop();
            continue; }
        ix += container_offset;
        // ix is past all container marks
        ix = parse_new_containers(&mut tree, s, ix);
        ix = parse_blocks(&mut tree, s, ix);
    }
    tree
}

fn get_html_end_tag(text : &str) -> Option<&'static str> {
    static BEGIN_TAGS: &'static [&'static str; 3] = &["<script", "<pre", "<style"];
    static END_TAGS: &'static [&'static str; 3] = &["</script>", "</pre>", "</style>"];

    // TODO: Consider using `strcasecmp` here
    'type_1: for (beg_tag, end_tag) in BEGIN_TAGS.iter().zip(END_TAGS.iter()) {
        if text.len() >= beg_tag.len() && text.starts_with("<") {
            for (i, c) in beg_tag.as_bytes()[1..].iter().enumerate() {
                if ! (&text.as_bytes()[i+1] == c || &text.as_bytes()[i+1] == &(c - 32)) {
                    continue 'type_1;
                }
            }

            // Must either be the end of the line...
            if text.len() == beg_tag.len() {
                return Some(end_tag);
            }

            // ...or be followed by whitespace, newline, or '>'.
            let pos = beg_tag.len();
            let s = text.as_bytes()[pos] as char;
            // TODO: I think this should be ASCII whitespace only
            if s.is_whitespace() || s == '>' {
                return Some(end_tag);
            }
        }
    }
    static ST_BEGIN_TAGS: &'static [&'static str; 3] = &["<!--", "<?", "<![CDATA["];
    static ST_END_TAGS: &'static [&'static str; 3] = &["-->", "?>", "]]>"];
    for (beg_tag, end_tag) in ST_BEGIN_TAGS.iter().zip(ST_END_TAGS.iter()) {
        if text.starts_with(&beg_tag[..]) {
            return Some(end_tag);
        }
    }
    if text.len() > 2 &&
        text.starts_with("<!") {
        let c = text[2..].chars().next().unwrap();
        if c >= 'A' && c <= 'Z' {
            return Some(">");
        }
    }
    None
}

#[derive(Copy, Clone, Debug)]
struct InlineEl {
    start: usize,  // offset of tree node
    count: usize,
    c: u8,  // b'*' or b'_'
    both: bool,  // can both open and close
}

#[derive(Debug)]
struct InlineStack {
    stack: Vec<InlineEl>,
}

impl InlineStack {
    fn new() -> InlineStack {
        InlineStack {
            stack: Vec::new(),
        }
    }

    fn pop_to(&mut self, tree: &mut Tree<Item>, new_len: usize) {
        while self.stack.len() > new_len {
            let el = self.stack.pop().unwrap();
            for i in 0..el.count {
                tree.nodes[el.start + i].item.body = ItemBody::Text;
            }
        }
    }

    fn find_match(&self, c: u8, count: usize, both: bool) -> Option<(usize, InlineEl)> {
        for (j, el) in self.stack.iter().enumerate().rev() {
            if el.c == c && !((both || el.both) && (count + el.count) % 3 == 0) {
                return Some((j, *el));
            }
        }
        None
    }

    fn push(&mut self, el: InlineEl) {
        self.stack.push(el)
    }

    fn pop(&mut self) -> Option<InlineEl> {
        self.stack.pop()
    }
}

/// An iterator for text in an inline chain.
#[derive(Clone)]
struct InlineScanner<'a> {
    tree: &'a Tree<Item>,
    text: &'a str,
    cur: usize,
    ix: usize,
}

impl<'a> InlineScanner<'a> {
    fn new(tree: &'a Tree<Item>, text: &'a str, cur: usize) -> InlineScanner<'a> {
        let ix = if cur == NIL { !0 } else { tree.nodes[cur].item.start };
        InlineScanner { tree, text, cur, ix }
    }

    fn unget(&mut self) {
        self.ix -= 1;
    }

    fn scan_ch(&mut self, c: u8) -> bool {
        self.scan_if(|scanned| scanned == c)
    }

    // Note(optimization): could use memchr
    fn scan_upto(&mut self, c: u8) -> usize {
        self.scan_while(|scanned| scanned != c)
    }

    fn scan_if<F>(&mut self, f: F) -> bool
        where F: Fn(u8) -> bool
    {
        if let Some(c) = self.next() {
            if !f(c) {
                self.unget();
            } else {
                return true;
            }
        }
        false
    }

    fn scan_while<F>(&mut self, f: F) -> usize
        where F: Fn(u8) -> bool
    {
        let mut n = 0;
        while let Some(c) = self.next() {
            if !f(c) {
                self.unget();
                break;
            }
            n += 1;
        }
        n
    }

    // Note: will consume the prefix of the string.
    fn scan_str(&mut self, s: &str) -> bool {
        s.as_bytes().iter().all(|b| self.scan_ch(*b))
    }

    fn to_node_and_ix(&self) -> (usize, usize) {
        let mut cur = self.cur;
        if cur != NIL && self.tree.nodes[cur].item.end == self.ix {
            cur = self.tree.nodes[cur].next;
        }
        (cur, self.ix)
    }

    fn next_char(&mut self) -> Option<char> {
        if self.cur == NIL { return None; }
        while self.ix == self.tree.nodes[self.cur].item.end {
            self.cur = self.tree.nodes[self.cur].next;
            if self.cur == NIL { return None; }
            self.ix = self.tree.nodes[self.cur].item.start;
        }
        self.text[self.ix..].chars().next().map(|c| {
            self.ix += c.len_utf8();
            c
        })
    }
}

impl<'a> Iterator for InlineScanner<'a> {
    type Item = u8;

    fn next(&mut self) -> Option<u8> {
        if self.cur == NIL { return None; }
        while self.ix == self.tree.nodes[self.cur].item.end {
            self.cur = self.tree.nodes[self.cur].next;
            if self.cur == NIL { return None; }
            self.ix = self.tree.nodes[self.cur].item.start;
        }
        let c = self.text.as_bytes()[self.ix];
        self.ix += 1;
        Some(c)
    }
}

fn scan_inline_attribute_name(scanner: &mut InlineScanner) -> bool {
    if !scanner.scan_if(|c| is_ascii_alpha(c) || c == b'_' || c == b':') {
        return false;
    }
    scanner.scan_while(|c| is_ascii_alphanumeric(c)
        || c == b'_' || c == b'.' || c == b':' || c == b'-');
    true
}

fn scan_inline_attribute_value(scanner: &mut InlineScanner) -> bool {
    if let Some(c) = scanner.next() {
        if is_ascii_whitespace(c) || c == b'=' || c == b'<' || c == b'>' || c == b'`' {
            scanner.unget();
        } else if c == b'\'' {
            scanner.scan_while(|c| c != b'\'');
            return scanner.scan_ch(b'\'')
        } else if c == b'"' {
            scanner.scan_while(|c| c != b'"');
            return scanner.scan_ch(b'"')
        } else {
            scanner.scan_while(|c| !(is_ascii_whitespace(c)
                || c == b'=' || c == b'<' || c == b'>' || c == b'`' || c == b'\'' || c == b'"'));
            return true;
        }
    }
    false
}

fn scan_inline_attribute(scanner: &mut InlineScanner) -> bool {
    if !scan_inline_attribute_name(scanner) { return false; }
    let n_whitespace = scanner.scan_while(is_ascii_whitespace);
    if scanner.scan_ch(b'=') {
        scanner.scan_while(is_ascii_whitespace);
        return scan_inline_attribute_value(scanner);
    } else if n_whitespace > 0 {
        // Leave whitespace for next attribute.
        scanner.unget();
    }
    true
}

/// Scan comment, declaration, or CDATA section, with initial "<!" already consumed.
fn scan_inline_html_comment(scanner: &mut InlineScanner) -> bool {
    if let Some(c) = scanner.next() {
        if c == b'-' {
            if !scanner.scan_ch(b'-') { return false; }
            // Saw "<!--", scan comment.
            if scanner.scan_ch(b'>') { return false; }
            if scanner.scan_ch(b'-') {
                if scanner.scan_ch(b'>') {
                    return false;
                } else {
                    scanner.unget();
                }
            }
            while scanner.scan_upto(b'-') > 0 {
                scanner.scan_ch(b'-');
                if scanner.scan_ch(b'-') { return scanner.scan_ch(b'>'); }
            }
        } else if c == b'[' {
            if !scanner.scan_str("CDATA[") { return false; }
            loop {
                scanner.scan_upto(b']');
                if !scanner.scan_ch(b']') { return false; }
                if scanner.scan_while(|c| c == b']') > 0 && scanner.scan_ch(b'>') {
                    return true;
                }
            }
        } else {
            // Scan declaration.
            if scanner.scan_while(|c| c >= b'A' && c <= b'Z') == 0 { return false; }
            if scanner.scan_while(is_ascii_whitespace) == 0 { return false; }
            scanner.scan_upto(b'>');
            return scanner.scan_ch(b'>');
        }
    }
    false
}

/// Scan processing directive, with initial "<?" already consumed.
fn scan_inline_html_processing(scanner: &mut InlineScanner) -> bool {
    while let Some(c) = scanner.next() {
        if c == b'?' && scanner.scan_ch(b'>') { return true; }
    }
    false
}

fn scan_inline_html(scanner: &mut InlineScanner) -> bool {
    if let Some(c) = scanner.next() {
        if c == b'!' {
            return scan_inline_html_comment(scanner);
        } else if c == b'?' {
            return scan_inline_html_processing(scanner);
        } else if c == b'/' {
            if !scanner.scan_if(is_ascii_alpha) {
                return false;
            }
            scanner.scan_while(is_ascii_letterdigitdash);
            scanner.scan_while(is_ascii_whitespace);
            return scanner.scan_ch(b'>');
        } else if is_ascii_alpha(c) {
            // open tag (first character of tag consumed)
            scanner.scan_while(is_ascii_letterdigitdash);
            loop {
                let n_whitespace = scanner.scan_while(is_ascii_whitespace);
                if let Some(c) = scanner.next() {
                    if c == b'/' {
                        return scanner.scan_ch(b'>');
                    } else if c == b'>' {
                        return true;
                    } else if n_whitespace == 0 {
                        return false;
                    } else {
                        scanner.unget();
                        if !scan_inline_attribute(scanner) {
                            return false;
                        }
                    }
                } else {
                    return false;
                }
            }
        }
    }
    false
}

/// Make a code span.
///
/// Both `open` and `close` are matching MaybeCode items.
fn make_code_span(tree: &mut Tree<Item>, s: &str, open: usize, close: usize) {
    tree.nodes[open].item.end = tree.nodes[close].item.end;
    tree.nodes[open].item.body = ItemBody::Code;
    let first = tree.nodes[open].next;
    tree.nodes[open].next = tree.nodes[close].next;
    tree.nodes[open].child = first;
    let mut node = first;
    let last;
    loop {
        let next = tree.nodes[node].next;
        match tree.nodes[node].item.body {
            ItemBody::SoftBreak => {
                // TODO: trailing space is stripped in parse_line, and we don't want it
                // stripped.
                tree.nodes[node].item.body = ItemBody::SynthesizeText(Borrowed(" "));
            }
            ItemBody::HardBreak => {
                let start = tree.nodes[node].item.start;
                if s.as_bytes()[start] == b'\\' {
                    tree.nodes[node].item.body = ItemBody::Text;
                    let end = tree.nodes[node].item.end;
                    let space = tree.create_node(Item {
                        start: start + 1,
                        end,
                        body: ItemBody::SynthesizeText(Borrowed(" "))
                    });
                    tree.nodes[space].next = next;
                    tree.nodes[node].next = space;
                    tree.nodes[node].item.end = start + 1;
                } else {
                    tree.nodes[node].item.body = ItemBody::SynthesizeText(Borrowed(" "));
                }
            }
            _ => tree.nodes[node].item.body = ItemBody::Text,
        }
        if next == close {
            last = node;
            tree.nodes[node].next = NIL;
            break;
        }
        node = next;
    }
    // Strip opening and closing space, if appropriate.
    let opening = match &tree.nodes[first].item.body {
        ItemBody::Text => s.as_bytes()[tree.nodes[first].item.start] == b' ',
        ItemBody::SynthesizeText(text) => text.starts_with(' '),
        _ => unreachable!("unexpected item"),
    };
    let closing = match &tree.nodes[last].item.body {
        ItemBody::Text => s.as_bytes()[tree.nodes[last].item.end - 1] == b' ',
        ItemBody::SynthesizeText(text) => text.ends_with(' '),
        _ => unreachable!("unexpected item"),
    };
    // TODO(spec clarification): This makes n-2 spaces for n spaces input. Correct?
    if opening && closing {
        if tree.nodes[first].item.body == ItemBody::SynthesizeText(Borrowed(" "))
            || tree.nodes[first].item.end - tree.nodes[first].item.start == 1
        {
            tree.nodes[open].child = tree.nodes[first].next;
        } else {
            tree.nodes[first].item.start += 1;
        }
        if tree.nodes[last].item.body == ItemBody::SynthesizeText(Borrowed(" ")) {
            tree.nodes[last].item.body = ItemBody::SynthesizeText(Borrowed(""));
        } else {
            tree.nodes[last].item.end -= 1;
        }
        // TODO: if last is now empty, remove it (we have size-0 items in the tree)
    }
}

fn scan_link_destination_plain(scanner: &mut InlineScanner) -> Option<String> {
    let mut url = String::new();
    let mut nest = 0;
    while let Some(c) = scanner.next_char() {
        match c {
            '(' => {
                url.push(c);
                nest += 1;
            }
            ')' => {
                if nest == 0 {
                    scanner.unget();
                    return Some(url);
                }
                url.push(c);
                nest -= 1;
            }
            '\x00'..=' ' => {
                scanner.unget();
                return Some(url);
            },
            '\\' => {
                if let Some(c) = scanner.next_char() {
                    if !(c <= '\x7f' && is_ascii_punctuation(c as u8)) {
                        url.push('\\');
                    }
                    url.push(c);
                } else {
                    return None;
                }
            }
            _ => url.push(c),
        }
    }
    None
}

fn scan_link_destination_pointy(scanner: &mut InlineScanner) -> Option<String> {
    if !scanner.scan_ch(b'<') {
        return None;
    }
    let mut url = String::new();
    while let Some(c) = scanner.next_char() {
        match c {
            '>' => return Some(url),
            '\x00'..='\x1f' | '<' => return None,
            '\\' => {
                let c = scanner.next_char()?;
                if !(c <= '\x7f' && is_ascii_punctuation(c as u8)) {
                    url.push('\\');
                }
                url.push(c);
            }
            _ => url.push(c),
        }
    }
    None
}

fn scan_link_destination(scanner: &mut InlineScanner) -> Option<String> {
    let save = scanner.clone();
    if let Some(url) = scan_link_destination_pointy(scanner) {
        return Some(url);
    }
    *scanner = save;
    scan_link_destination_plain(scanner)
}

fn scan_link_title(scanner: &mut InlineScanner) -> Option<String> {
    let open = scanner.next_char()?;
    if !(open == '\'' || open == '\"' || open == '(') {
        return None;
    }
    let mut title = String::new();
    let mut nest = 0;
    while let Some(c) = scanner.next_char() {
        if c == open {
            if open == '(' {
                nest += 1;
            } else {
                return Some(title);
            }
        }
        if open == '(' && c == ')' {
            if nest == 0 {
                return Some(title);
            } else {
                nest -= 1;
            }
        }
        if c == '\\' {
            let c = scanner.next_char()?;
            if !(c <= '\x7f' && is_ascii_punctuation(c as u8)) {
                title.push('\\');
            }
            title.push(c);
        } else {
            title.push(c);
        }
    }
    None
}

fn scan_inline_link(scanner: &mut InlineScanner) -> Option<(String, String)> {
    if !scanner.scan_ch(b'(') {
        return None;
    }
    scanner.scan_while(is_ascii_whitespace);
    let url = scan_link_destination(scanner)?;
    let mut title = String::new();
    let save = scanner.clone();
    if scanner.scan_while(is_ascii_whitespace) > 0 {
        if let Some(t) = scan_link_title(scanner) {
            title = t;
            scanner.scan_while(is_ascii_whitespace);
        } else {
            *scanner = save;
        }
    }
    if !scanner.scan_ch(b')') {
        return None;
    }
    Some((url, title))
}

// TODO: if it's just node, get rid of struct. But I think there probably will be
// state related to normalization.
struct LinkStackEl {
    node: usize,
}

/// Handle inline HTML, code spans, and links.
///
/// This function handles both inline HTML and code spans, because they have
/// the same precedence. It also handles links, even though they have lower
/// precedence, because the URL of links must not be processed.
fn handle_inline_pass1(tree: &mut Tree<Item>, s: &str) {
    let mut link_stack = Vec::new();
    let mut cur = tree.cur;
    let mut prev = NIL;
    while cur != NIL {
        match tree.nodes[cur].item.body {
            ItemBody::MaybeHtml => {
                let maybe_html = {
                    let next = tree.nodes[cur].next;
                    let mut scanner = InlineScanner::new(tree, s, next);
                    if scan_inline_html(&mut scanner) {
                        Some(scanner.to_node_and_ix())
                    } else {
                        None
                    }
                };
                if let Some((node, ix)) = maybe_html {
                    // TODO: this logic isn't right if the replaced chain has
                    // tricky stuff (skipped containers, replaced nulls).
                    tree.nodes[cur].item.body = ItemBody::InlineHtml;
                    tree.nodes[cur].item.end = ix;
                    tree.nodes[cur].next = node;
                    cur = node;
                    if cur != NIL {
                        tree.nodes[cur].item.start = ix;
                    }
                    continue;
                }
                tree.nodes[cur].item.body = ItemBody::Text;
            }
            ItemBody::MaybeCode(count) => {
                // TODO(performance): this has quadratic pathological behavior, I think
                let first = tree.nodes[cur].next;
                let mut scan = first;
                while scan != NIL {
                    if tree.nodes[scan].item.body == ItemBody::MaybeCode(count) {
                        make_code_span(tree, s, cur, scan);
                        break;
                    }
                    scan = tree.nodes[scan].next;
                }
                if scan == NIL {
                    tree.nodes[cur].item.body = ItemBody::Text;
                }
            }
            ItemBody::MaybeLinkOpen => {
                tree.nodes[cur].item.body = ItemBody::Text;
                link_stack.push( LinkStackEl { node: cur });
            }
            ItemBody::MaybeLinkClose => {
                let made_link = if let Some(tos) = link_stack.last() {
                    let next = tree.nodes[cur].next;
                    let (link_info, (next_node, next_ix)) = {
                        let mut scanner = InlineScanner::new(tree, s, next);
                        (scan_inline_link(&mut scanner), scanner.to_node_and_ix())
                    };
                    if let Some((url, title)) = link_info {
                        tree.nodes[prev].next = NIL;
                        cur = tos.node;
                        tree.nodes[cur].item.body = ItemBody::Link(url.into(), title.into());
                        tree.nodes[cur].child = tree.nodes[cur].next;
                        tree.nodes[cur].next = next_node;
                        if next_node != NIL {
                            tree.nodes[next_node].item.start = next_ix;
                        }
                        true
                    } else {
                        false
                    }
                } else {
                    false
                };
                if made_link {
                    link_stack.clear();
                } else {
                    tree.nodes[cur].item.body = ItemBody::Text;
                }
            }
            _ => (),
        }
        prev = cur;
        cur = tree.nodes[cur].next;
    }
}

fn handle_emphasis(tree: &mut Tree<Item>, s: &str) {
    let mut stack = InlineStack::new();
    let mut prev = NIL;
    let mut cur = tree.cur;
    while cur != NIL {
        if let ItemBody::Inline(mut count, can_open, can_close) = tree.nodes[cur].item.body {
            let c = s.as_bytes()[tree.nodes[cur].item.start];
            let both = can_open && can_close;
            if can_close {
                while let Some((j, el)) = stack.find_match(c, count, both) {
                    // have a match!
                    tree.nodes[prev].next = NIL;
                    let match_count = ::std::cmp::min(count, el.count);
                    let mut end = cur + match_count;
                    cur = tree.nodes[end - 1].next;
                    let mut next = cur;
                    let mut start = el.start + el.count - match_count;
                    prev = start;
                    while start < el.start + el.count {
                        let (inc, ty) = if el.start + el.count - start > 1 {
                            (2, ItemBody::Strong)
                        } else {
                            (1, ItemBody::Emphasis)
                        };
                        let root = start + inc;
                        end -= inc;
                        tree.nodes[start].item.body = ty;
                        tree.nodes[start].item.end = tree.nodes[end].item.end;
                        tree.nodes[start].child = root;
                        tree.nodes[start].next = next;
                        start = root;
                        next = NIL;
                    }
                    stack.pop_to(tree, j + 1);
                    let _ = stack.pop();
                    if el.count > match_count {
                        stack.push(InlineEl {
                            start: el.start,
                            count: el.count - match_count,
                            c: el.c,
                            both: both,
                        })
                    }
                    count -= match_count;
                    if count == 0 {
                        break;
                    }
                }
            }
            if count > 0 {
                if can_open {
                    stack.push(InlineEl {
                        start: cur,
                        count: count,
                        c: c,
                        both: both,
                    });
                } else {
                    for i in 0..count {
                        tree.nodes[cur + i].item.body = ItemBody::Text;
                    }
                }
                prev = cur + count - 1;
                cur = tree.nodes[prev].next;
            }
        } else {
            prev = cur;
            cur = tree.nodes[cur].next;
        }
    }
    stack.pop_to(tree, 0);
}

/// Handle inline markup.
///
/// When the parser encounters any item indicating potential inline markup, all
/// inline markup passes are run on the remainder of the chain.
///
/// Note: there's some potential for optimization here, but that's future work.
fn handle_inline(tree: &mut Tree<Item>, s: &str) {
    handle_inline_pass1(tree, s);
    handle_emphasis(tree, s);
}

pub struct Parser<'a> {
    text: &'a str,
    tree: Tree<Item>,
}

impl<'a> Parser<'a> {
    pub fn new(text: &'a str) -> Parser<'a> {
        Parser::new_ext(text, Options::empty())
    }

    #[allow(unused_variables)]
    pub fn new_ext(text: &'a str, opts: Options) -> Parser<'a> {
        let first_pass = FirstPass::new(text);
        let mut tree = first_pass.run();
        tree.cur = if tree.nodes.is_empty() { NIL } else { 0 };
        tree.spine = vec![];
        Parser {
            text: text,
            tree: tree,
        }
    }

    pub fn get_offset(&self) -> usize {
        0  // TODO
    }
}

fn item_to_tag(item: &Item) -> Option<Tag<'static>> {
    match item.body {
        ItemBody::Paragraph => Some(Tag::Paragraph),
        ItemBody::Code => Some(Tag::Code),
        ItemBody::Emphasis => Some(Tag::Emphasis),
        ItemBody::Strong => Some(Tag::Strong),
        ItemBody::Link(ref url, ref title) =>
            Some(Tag::Link(url.clone().into(), title.clone().into())),
        ItemBody::Rule => Some(Tag::Rule),
        ItemBody::Header(level) => Some(Tag::Header(level)),
        ItemBody::FencedCodeBlock(ref info_string) =>
            Some(Tag::CodeBlock(info_string.clone().into())),
        ItemBody::IndentCodeBlock(_) => Some(Tag::CodeBlock("".into())),
        ItemBody::BlockQuote => Some(Tag::BlockQuote),
        ItemBody::List(_, _, listitem_start) => Some(Tag::List(listitem_start)),
        ItemBody::ListItem(_) => Some(Tag::Item),
        ItemBody::HtmlBlock(_) => Some(Tag::HtmlBlock),
        _ => None,
    }
}

// leaf items only
fn item_to_event<'a>(item: &Item, text: &'a str) -> Event<'a> {
    match item.body {
        ItemBody::Text => {
            Event::Text(Cow::from(&text[item.start..item.end]))
        },
        ItemBody::SynthesizeText(ref text) => {
            Event::Text(text.clone())
        }
        ItemBody::SynthesizeNewLine => {
            Event::Text(Cow::from("\n"))
        },
        ItemBody::BlankLine => {
            Event::Text(Cow::from(""))
        },
        ItemBody::Html => {
            Event::Html(Cow::from(&text[item.start..item.end]))
        },
        ItemBody::InlineHtml => {
            Event::InlineHtml(Cow::from(&text[item.start..item.end]))
        },
        ItemBody::SoftBreak => Event::SoftBreak,
        ItemBody::HardBreak => Event::HardBreak,
        _ => panic!("unexpected item body {:?}", item.body)
    }
}

#[allow(unused)]
// tree.cur points to a List<_, _, _> Item Node
fn detect_tight_list(tree: &Tree<Item>) -> bool {
    let mut this_listitem = tree.nodes[tree.cur].child;
    while this_listitem != NIL {
        let on_lastborn_child = tree.nodes[this_listitem].next == NIL;
        if let ItemBody::ListItem(_) = tree.nodes[this_listitem].item.body {
            let mut this_listitem_child = tree.nodes[this_listitem].child;
            let mut on_firstborn_grandchild = true; 
            if this_listitem_child != NIL {
                while this_listitem_child != NIL {
                    let on_lastborn_grandchild = tree.nodes[this_listitem_child].next == NIL;
                    if let ItemBody::BlankLine = tree.nodes[this_listitem_child].item.body {
                        // If the first line is blank, this does not trigger looseness.
                        // Blanklines at the very end of a list also do not trigger looseness.
                        if !on_firstborn_grandchild && !(on_lastborn_child && on_lastborn_grandchild) {  
                            return false;
                        }
                    }
                    on_firstborn_grandchild = false;
                    this_listitem_child = tree.nodes[this_listitem_child].next;
                }
            } // the else should panic!
        }

        this_listitem = tree.nodes[this_listitem].next;
    }
    return true;
}

// https://english.stackexchange.com/a/285573
// tree.cur points to a List<_, _, _, false> Item Node
fn surgerize_tight_list(tree : &mut Tree<Item>) {
    let mut this_listitem = tree.nodes[tree.cur].child;
    while this_listitem != NIL {
        if let ItemBody::ListItem(_) = tree.nodes[this_listitem].item.body {
            // first child is special, controls how we repoint this_listitem.child
            let this_listitem_firstborn = tree.nodes[this_listitem].child;
            if this_listitem_firstborn != NIL {
                if let ItemBody::Paragraph = tree.nodes[this_listitem_firstborn].item.body {
                    // paragraphs should always have children
                    tree.nodes[this_listitem].child = tree.nodes[this_listitem_firstborn].child;
                }

                let mut this_listitem_child = this_listitem_firstborn;
                let mut node_to_repoint = NIL;
                while this_listitem_child != NIL {
                    // surgerize paragraphs
                    if let ItemBody::Paragraph = tree.nodes[this_listitem_child].item.body {
                        let this_listitem_child_firstborn = tree.nodes[this_listitem_child].child;
                        if node_to_repoint != NIL {
                            tree.nodes[node_to_repoint].next = this_listitem_child_firstborn;
                        }
                        let mut this_listitem_child_lastborn = this_listitem_child_firstborn;
                        while tree.nodes[this_listitem_child_lastborn].next != NIL {
                            this_listitem_child_lastborn = tree.nodes[this_listitem_child_lastborn].next;
                        }
                        node_to_repoint = this_listitem_child_lastborn;
                    } else {
                        node_to_repoint = this_listitem_child;
                    }

                    tree.nodes[node_to_repoint].next = tree.nodes[this_listitem_child].next;
                    this_listitem_child = tree.nodes[this_listitem_child].next;
                }
            } // listitems should always have children, let this pass during testing
        } // failure should be a panic, but I'll let it pass during testing

        this_listitem = tree.nodes[this_listitem].next;
    }
}

impl<'a> Iterator for Parser<'a> {
    type Item = Event<'a>;

    fn next(&mut self) -> Option<Event<'a>> {
        if self.tree.cur == NIL {
            if let Some(cur) = self.tree.spine.pop() {
                let tag = item_to_tag(&self.tree.nodes[cur].item).unwrap();
                self.tree.cur = self.tree.nodes[cur].next;
                return Some(Event::End(tag));
            } else {
                return None;
            }
        }
        match self.tree.nodes[self.tree.cur].item.body {
            ItemBody::Inline(..) | ItemBody::MaybeHtml | ItemBody::MaybeCode(_)
            | ItemBody::MaybeLinkOpen | ItemBody::MaybeLinkClose =>
                handle_inline(&mut self.tree, self.text),
            ItemBody::Backslash => self.tree.cur = self.tree.nodes[self.tree.cur].next,
            _ => (),
        }
        let item = &self.tree.nodes[self.tree.cur].item;
        if let Some(tag) = item_to_tag(item) {
            let child = self.tree.nodes[self.tree.cur].child;
            self.tree.spine.push(self.tree.cur);
            self.tree.cur = child;
            return Some(Event::Start(tag))
        } else {
            self.tree.cur = self.tree.nodes[self.tree.cur].next;
            return Some(item_to_event(item, self.text))
        }
    }
}

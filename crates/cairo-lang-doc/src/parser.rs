use std::fmt;

use cairo_lang_debug::DebugWithDb;
use cairo_lang_defs::ids::{
    FileIndex, GenericTypeId, LookupItemId, ModuleFileId, ModuleId, ModuleItemId, TraitItemId,
};
use cairo_lang_diagnostics::DiagnosticsBuilder;
use cairo_lang_filesystem::ids::{FileKind, FileLongId, VirtualFile};
use cairo_lang_parser::parser::Parser;
use cairo_lang_semantic::db::SemanticGroup;
use cairo_lang_semantic::diagnostic::{NotFoundItemType, SemanticDiagnostics};
use cairo_lang_semantic::expr::inference::InferenceId;
use cairo_lang_semantic::items::functions::GenericFunctionId;
use cairo_lang_semantic::resolve::{AsSegments, ResolutionContext, ResolvedGenericItem, Resolver};
use cairo_lang_syntax::node::ast::{Expr, ExprPath, ItemModule};
use cairo_lang_syntax::node::helpers::GetIdentifier;
use cairo_lang_syntax::node::{SyntaxNode, TypedSyntaxNode};
use cairo_lang_utils::Intern;
use pulldown_cmark::{
    BrokenLink, CodeBlockKind, Event, HeadingLevel, LinkType, Options, Parser as MarkdownParser,
    Tag, TagEnd,
};

use crate::db::DocGroup;
use crate::documentable_item::DocumentableItemId;

/// Token representing a link to another item inside the documentation.
#[derive(Debug, PartialEq, Clone, Eq)]
pub struct CommentLinkToken {
    /// A link part that's inside "[]" brackets.
    pub label: String,
    /// A link part that's inside "()" brackets, right after the label.
    pub path: Option<String>,
    /// Item resolved based on the path provided by user. If resolver cannot resolve the item, we
    /// leave it as None.
    pub resolved_item: Option<DocumentableItemId>,
}

/// Generic type for a comment token. It's either a plain content or a link.
/// Notice that the Content type of token can store much more than just one word.
#[derive(Debug, PartialEq, Clone, Eq)]
pub enum DocumentationCommentToken {
    /// Token with plain documentation content.
    Content(String),
    /// Link token.
    Link(CommentLinkToken),
}

impl DocumentationCommentToken {
    /// Checks if string representation of [`DocumentationCommentToken`] ends with newline.
    pub fn ends_with_newline(self) -> bool {
        match self {
            DocumentationCommentToken::Content(content) => content.ends_with('\n'),
            DocumentationCommentToken::Link(link_token) => link_token.label.ends_with('\n'),
        }
    }
}

/// Helper struct for formatting possibly nested Markdown lists.
struct DocCommentListItem {
    /// Order list item separator
    delimiter: Option<u64>,
    /// Flag for a list with order elements
    is_ordered_list: bool,
}

/// Parses plain documentation comments into [DocumentationCommentToken]s.
pub struct DocumentationCommentParser<'a> {
    db: &'a dyn DocGroup,
}

impl<'a> DocumentationCommentParser<'a> {
    pub fn new(db: &'a dyn DocGroup) -> Self {
        Self { db }
    }

    /// Parses documentation comment content into vector of [DocumentationCommentToken]s, keeping
    /// the order in which they were present in the content.
    ///
    /// We look for 3 types of patterns when it comes to link (ignore the backslash):
    /// "\[label\](path)", "\[path\]" or "\[`path`\]".
    pub fn parse_documentation_comment(
        &self,
        item_id: DocumentableItemId,
        documentation_comment: String,
    ) -> Vec<DocumentationCommentToken> {
        let mut tokens = Vec::new();
        let mut current_link: Option<CommentLinkToken> = None;
        let mut is_indented_code_block = false;
        let mut replacer = |broken_link: BrokenLink<'_>| {
            if matches!(broken_link.link_type, LinkType::ShortcutUnknown | LinkType::Shortcut) {
                return Some((broken_link.reference.to_string().into(), "".into()));
            }
            None
        };

        let parser = MarkdownParser::new_with_broken_link_callback(
            &documentation_comment,
            Options::empty(),
            Some(&mut replacer),
        );

        let mut list_nesting: Vec<DocCommentListItem> = Vec::new();
        let write_list_item_prefix =
            |list_nesting: &mut Vec<DocCommentListItem>,
             tokens: &mut Vec<DocumentationCommentToken>| {
                if !list_nesting.is_empty() {
                    let indent = "  ".repeat(list_nesting.len() - 1);
                    let list_nesting = list_nesting.last_mut().unwrap();

                    let item_delimiter = if list_nesting.is_ordered_list {
                        let delimiter = list_nesting.delimiter.unwrap_or(0);
                        list_nesting.delimiter = Some(delimiter + 1);
                        format!("{indent}{delimiter}.",)
                    } else {
                        format!("{indent}-")
                    };
                    tokens.push(DocumentationCommentToken::Content(format!(
                        "{indent}{item_delimiter} "
                    )));
                }
            };
        let mut prefix_list_item = false;

        let mut last_two_events = [None, None];

        for event in parser {
            let current_event = event.clone();
            match current_event {
                Event::Text(text) => {
                    if prefix_list_item {
                        write_list_item_prefix(&mut list_nesting, &mut tokens);
                        prefix_list_item = false;
                    }
                    if let Some(link) = current_link.as_mut() {
                        link.label.push_str(&text);
                    } else {
                        let text = {
                            if is_indented_code_block {
                                format!("    {}", text)
                            } else {
                                text.to_string()
                            }
                        };
                        tokens.push(DocumentationCommentToken::Content(text));
                    }
                }
                Event::Code(code) => {
                    if prefix_list_item {
                        write_list_item_prefix(&mut list_nesting, &mut tokens);
                        prefix_list_item = false;
                    }
                    let complete_code = format!("`{}`", code);
                    if let Some(link) = current_link.as_mut() {
                        link.label.push_str(&complete_code);
                    } else {
                        tokens.push(DocumentationCommentToken::Content(complete_code));
                    }
                }
                Event::Start(tag_start) => {
                    match tag_start {
                        Tag::Heading { level, .. } => {
                            if let Some(last_token) = tokens.last_mut() {
                                if !last_token.clone().ends_with_newline() {
                                    tokens
                                        .push(DocumentationCommentToken::Content("\n".to_string()));
                                }
                            }
                            tokens.push(DocumentationCommentToken::Content(format!(
                                "{} ",
                                heading_level_to_markdown(level)
                            )));
                        }
                        Tag::List(list_type) => {
                            tokens.push(DocumentationCommentToken::Content("\n".to_string()));
                            list_nesting.push(DocCommentListItem {
                                delimiter: list_type,
                                is_ordered_list: list_type.is_some(),
                            });
                        }
                        Tag::CodeBlock(kind) => match kind {
                            CodeBlockKind::Fenced(language) => {
                                if language.trim().is_empty() {
                                    tokens.push(DocumentationCommentToken::Content(String::from(
                                        "\n```cairo\n",
                                    )));
                                } else {
                                    tokens.push(DocumentationCommentToken::Content(format!(
                                        "\n```{}\n",
                                        language
                                    )));
                                }
                            }
                            CodeBlockKind::Indented => {
                                tokens.push(DocumentationCommentToken::Content("\n\n".to_string()));
                                is_indented_code_block = true;
                            }
                        },
                        Tag::Link { link_type, dest_url, .. } => {
                            match link_type {
                                LinkType::ShortcutUnknown | LinkType::Shortcut => {
                                    let path =
                                        if dest_url.starts_with("`") && dest_url.ends_with("`") {
                                            dest_url
                                                .trim_start_matches("`")
                                                .trim_end_matches("`")
                                                .to_string()
                                        } else {
                                            dest_url.clone().to_string()
                                        };
                                    current_link = Some(CommentLinkToken {
                                        label: "".to_string(),
                                        path: None,
                                        resolved_item: self.resolve_linked_item(item_id, path), /* Or resolve item here */
                                    });
                                }
                                _ => {
                                    current_link = Some(CommentLinkToken {
                                        label: "".to_string(),
                                        path: Some(dest_url.clone().into_string()),
                                        resolved_item: self.resolve_linked_item(
                                            item_id,
                                            dest_url.clone().into_string(),
                                        ), // Or resolve item here
                                    });
                                }
                            }
                        }
                        Tag::Paragraph => {
                            tokens.push(DocumentationCommentToken::Content("\n".to_string()));
                        }
                        Tag::Item => {
                            prefix_list_item = true;
                        }
                        _ => {}
                    }
                }
                Event::End(tag_end) => match tag_end {
                    TagEnd::Heading(_) => {
                        tokens.push(DocumentationCommentToken::Content("\n".to_string()));
                    }
                    TagEnd::List(_) => {
                        list_nesting.pop();
                    }
                    TagEnd::Item => {
                        if !matches!(last_two_events[0], Some(Event::End(_)))
                            | !matches!(last_two_events[1], Some(Event::End(_)))
                        {
                            tokens.push(DocumentationCommentToken::Content("\n".to_string()));
                        }
                    }
                    TagEnd::CodeBlock => {
                        if !is_indented_code_block {
                            tokens.push(DocumentationCommentToken::Content("```\n".to_string()));
                        }
                        is_indented_code_block = false;
                    }
                    TagEnd::Link => {
                        if let Some(link) = current_link.take() {
                            tokens.push(DocumentationCommentToken::Link(link));
                        }
                    }
                    _ => {}
                },
                Event::SoftBreak => {
                    tokens.push(DocumentationCommentToken::Content("\n".to_string()));
                }
                _ => {}
            }
            last_two_events = [last_two_events[1].clone(), Some(event)];
        }

        if let Some(DocumentationCommentToken::Content(token)) = tokens.first() {
            if token == "\n" {
                tokens.remove(0);
            }
        }
        if let Some(DocumentationCommentToken::Content(token)) = tokens.last_mut() {
            *token = token.trim_end().to_string();
            if token.is_empty() {
                tokens.pop();
            }
        }

        tokens
    }

    /// Resolves item based on the provided path as a string.
    fn resolve_linked_item(
        &self,
        item_id: DocumentableItemId,
        path: String,
    ) -> Option<DocumentableItemId> {
        let syntax_node = item_id.stable_location(self.db.upcast())?.syntax_node(self.db.upcast());
        let containing_module = self.find_module_file_containing_node(&syntax_node)?;
        let mut resolver =
            Resolver::new(self.db.upcast(), containing_module, InferenceId::NoContext);
        let mut diagnostics = SemanticDiagnostics::default();
        let segments = self.parse_comment_link_path(path)?;
        resolver
            .resolve_generic_path(
                &mut diagnostics,
                segments.to_segments(self.db.upcast()),
                NotFoundItemType::Identifier,
                ResolutionContext::Default,
            )
            .ok()?
            .to_documentable_item_id(self.db.upcast())
    }

    /// Parses the path as a string to a Path Expression, which can be later used by a resolver.
    fn parse_comment_link_path(&self, path: String) -> Option<ExprPath> {
        let virtual_file = FileLongId::Virtual(VirtualFile {
            parent: Default::default(),
            name: Default::default(),
            content: Default::default(),
            code_mappings: Default::default(),
            kind: FileKind::Module,
        })
        .intern(self.db);

        let expr = Parser::parse_file_expr(
            self.db.upcast(),
            &mut DiagnosticsBuilder::default(),
            virtual_file,
            &path,
        );

        if let Expr::Path(expr_path) = expr { Some(expr_path) } else { None }
    }

    /// Returns a [`ModuleFileId`] containing the node.
    ///
    /// If the node is located in a virtual file generated by a compiler plugin, this method will
    /// return a [`ModuleFileId`] pointing to the main, user-written file of the module.
    fn find_module_file_containing_node(&self, node: &SyntaxNode) -> Option<ModuleFileId> {
        let module_id = self.find_module_containing_node(node)?;
        let file_index = FileIndex(0);
        Some(ModuleFileId(module_id, file_index))
    }
    /// Finds a [`ModuleId`] containing the node.
    ///
    /// If the node is located in a virtual file generated by a compiler plugin, this method will
    /// return the (sub)module of the main, user-written file that leads to the node.
    fn find_module_containing_node(&self, node: &SyntaxNode) -> Option<ModuleId> {
        let db: &dyn SemanticGroup = self.db.upcast();
        let syntax_db = db.upcast();

        // Get the main module of the main file that leads to the node.
        // The node may be located in a virtual file of a submodule.
        // This code attempts to get the absolute "parent" of both "module" and "file" parts.
        let main_module = {
            // Get the file where the node is located.
            // This might be a virtual file generated by a compiler plugin.
            let node_file_id = node.stable_ptr().file_id(syntax_db);

            // Get the root module of a file containing the node.
            let node_main_module = db.file_modules(node_file_id).ok()?.iter().copied().next()?;

            // Get the main module of the file.
            let main_file = db.module_main_file(node_main_module).ok()?;

            // Get the main module of that file.
            db.file_modules(main_file).ok()?.iter().copied().next()?
        };

        // Get the stack (bottom-up) of submodule names in the file containing the node, in the main
        // module, that lead to the node.
        node.ancestors()
            .filter_map(|node| ItemModule::cast(syntax_db, node))
            .map(|item_module| {
                item_module
                    .stable_ptr()
                    .name_green(syntax_db)
                    .identifier(syntax_db)
            })
            // Buffer the stack to get DoubleEndedIterator.
            .collect::<Vec<_>>()
            .into_iter()
            // And get id of the (sub)module containing the node by traversing this stack top-down.
            .try_rfold(main_module, |module, name| {
                let ModuleItemId::Submodule(submodule) =
                    db.module_item_by_name(module, name).ok()??
                else {
                    return None;
                };
                Some(ModuleId::Submodule(submodule))
            })
    }
}

trait ToDocumentableItemId<T> {
    fn to_documentable_item_id(self, db: &dyn SemanticGroup) -> Option<DocumentableItemId>;
}

impl ToDocumentableItemId<DocumentableItemId> for ResolvedGenericItem {
    /// Converts the [ResolvedGenericItem] to [DocumentableItemId].
    /// As for now, returns None only for a common Variable, as those are not a supported
    /// documentable item.
    fn to_documentable_item_id(self, db: &dyn SemanticGroup) -> Option<DocumentableItemId> {
        match self {
            ResolvedGenericItem::GenericConstant(id) => Some(DocumentableItemId::LookupItem(
                LookupItemId::ModuleItem(ModuleItemId::Constant(id)),
            )),
            ResolvedGenericItem::GenericFunction(GenericFunctionId::Free(id)) => {
                Some(DocumentableItemId::LookupItem(LookupItemId::ModuleItem(
                    ModuleItemId::FreeFunction(id),
                )))
            }
            ResolvedGenericItem::GenericType(GenericTypeId::Struct(id)) => Some(
                DocumentableItemId::LookupItem(LookupItemId::ModuleItem(ModuleItemId::Struct(id))),
            ),
            ResolvedGenericItem::GenericType(GenericTypeId::Enum(id)) => Some(
                DocumentableItemId::LookupItem(LookupItemId::ModuleItem(ModuleItemId::Enum(id))),
            ),
            ResolvedGenericItem::GenericTypeAlias(id) => Some(DocumentableItemId::LookupItem(
                LookupItemId::ModuleItem(ModuleItemId::TypeAlias(id)),
            )),
            ResolvedGenericItem::GenericImplAlias(id) => Some(DocumentableItemId::LookupItem(
                LookupItemId::ModuleItem(ModuleItemId::ImplAlias(id)),
            )),
            ResolvedGenericItem::Trait(id) => Some(DocumentableItemId::LookupItem(
                LookupItemId::ModuleItem(ModuleItemId::Trait(id)),
            )),
            ResolvedGenericItem::Impl(id) => Some(DocumentableItemId::LookupItem(
                LookupItemId::ModuleItem(ModuleItemId::Impl(id)),
            )),
            ResolvedGenericItem::GenericType(GenericTypeId::Extern(id)) => {
                Some(DocumentableItemId::LookupItem(LookupItemId::ModuleItem(
                    ModuleItemId::ExternType(id),
                )))
            }
            ResolvedGenericItem::GenericFunction(GenericFunctionId::Extern(id)) => {
                Some(DocumentableItemId::LookupItem(LookupItemId::ModuleItem(
                    ModuleItemId::ExternFunction(id),
                )))
            }
            ResolvedGenericItem::Module(ModuleId::Submodule(id)) => {
                Some(DocumentableItemId::LookupItem(LookupItemId::ModuleItem(
                    ModuleItemId::Submodule(id),
                )))
            }
            ResolvedGenericItem::Module(ModuleId::CrateRoot(id)) => {
                Some(DocumentableItemId::Crate(id))
            }
            ResolvedGenericItem::Variant(variant) => Some(DocumentableItemId::Variant(variant.id)),
            ResolvedGenericItem::GenericFunction(GenericFunctionId::Impl(generic_impl_func)) => {
                if let Some(impl_function) = generic_impl_func.impl_function(db).ok().flatten() {
                    Some(DocumentableItemId::LookupItem(LookupItemId::ImplItem(
                        cairo_lang_defs::ids::ImplItemId::Function(impl_function),
                    )))
                } else {
                    Some(DocumentableItemId::LookupItem(LookupItemId::TraitItem(
                        TraitItemId::Function(generic_impl_func.function),
                    )))
                }
            }
            ResolvedGenericItem::TraitItem(id) => {
                Some(DocumentableItemId::LookupItem(LookupItemId::TraitItem(id)))
            }
            ResolvedGenericItem::Variable(_) => None,
        }
    }
}

impl fmt::Display for CommentLinkToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.path.clone() {
            Some(path) => write!(f, "[{}]({})", self.label, path),
            None => write!(f, "[{}]", self.label),
        }
    }
}

impl fmt::Display for DocumentationCommentToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DocumentationCommentToken::Content(ref content) => {
                write!(f, "{}", content)
            }
            DocumentationCommentToken::Link(ref link_token) => {
                write!(f, "{}", link_token)
            }
        }
    }
}

impl DebugWithDb<dyn DocGroup> for CommentLinkToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>, db: &dyn DocGroup) -> fmt::Result {
        f.debug_struct("CommentLinkToken")
            .field("label", &self.label)
            .field("path", &self.path)
            .field("resolved_item_name", &self.resolved_item.map(|item| item.name(db.upcast())))
            .finish()
    }
}

/// Maps `HeadingLevel` to correct markdown marker.
fn heading_level_to_markdown(heading_level: HeadingLevel) -> String {
    let heading_char: String = String::from("#");
    match heading_level {
        HeadingLevel::H1 => heading_char,
        HeadingLevel::H2 => heading_char.repeat(2),
        HeadingLevel::H3 => heading_char.repeat(3),
        HeadingLevel::H4 => heading_char.repeat(4),
        HeadingLevel::H5 => heading_char.repeat(5),
        HeadingLevel::H6 => heading_char.repeat(6),
    }
}

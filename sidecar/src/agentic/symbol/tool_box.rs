use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures::{stream, StreamExt};
use llm_client::clients::types::LLMType;
use llm_client::provider::{LLMProvider, LLMProviderAPIKeys};
use tokio::sync::mpsc::UnboundedSender;

use crate::agentic::symbol::helpers::split_file_content_into_parts;
use crate::agentic::symbol::identifier::{Snippet, SymbolIdentifier};
use crate::agentic::tool::base::Tool;
use crate::agentic::tool::code_edit::types::CodeEdit;
use crate::agentic::tool::code_symbol::correctness::{
    CodeCorrectnessAction, CodeCorrectnessRequest,
};
use crate::agentic::tool::code_symbol::error_fix::CodeEditingErrorRequest;
use crate::agentic::tool::code_symbol::followup::{
    ClassSymbolFollowupRequest, ClassSymbolFollowupResponse, ClassSymbolMember,
};
use crate::agentic::tool::code_symbol::important::{
    CodeSymbolImportantRequest, CodeSymbolImportantResponse, CodeSymbolToAskQuestionsRequest,
    CodeSymbolUtilityRequest, CodeSymbolWithThinking,
};
use crate::agentic::tool::code_symbol::models::anthropic::CodeSymbolShouldAskQuestionsResponse;
use crate::agentic::tool::editor::apply::{EditorApplyRequest, EditorApplyResponse};
use crate::agentic::tool::errors::ToolError;
use crate::agentic::tool::filtering::broker::{
    CodeToEditFilterRequest, CodeToEditFilterResponse, CodeToEditSymbolRequest,
    CodeToEditSymbolResponse, CodeToProbeFilterResponse,
};
use crate::agentic::tool::grep::file::{FindInFileRequest, FindInFileResponse};
use crate::agentic::tool::lsp::diagnostics::{
    Diagnostic, LSPDiagnosticsInput, LSPDiagnosticsOutput,
};
use crate::agentic::tool::lsp::gotodefintion::{GoToDefinitionRequest, GoToDefinitionResponse};
use crate::agentic::tool::lsp::gotoimplementations::{
    GoToImplementationRequest, GoToImplementationResponse,
};
use crate::agentic::tool::lsp::gotoreferences::{GoToReferencesRequest, GoToReferencesResponse};
use crate::agentic::tool::lsp::open_file::OpenFileResponse;
use crate::agentic::tool::lsp::quick_fix::{
    GetQuickFixRequest, GetQuickFixResponse, LSPQuickFixInvocationRequest,
    LSPQuickFixInvocationResponse, QuickFixOption,
};
use crate::chunking::editor_parsing::EditorParsing;
use crate::chunking::text_document::{Position, Range};
use crate::chunking::types::{OutlineNode, OutlineNodeContent};
use crate::user_context::types::UserContext;
use crate::{
    agentic::tool::{broker::ToolBroker, input::ToolInput, lsp::open_file::OpenFileRequest},
    inline_completion::symbols_tracker::SymbolTrackerInline,
};

use super::errors::SymbolError;
use super::events::edit::SymbolToEdit;
use super::events::probe::SymbolToProbeRequest;
use super::identifier::MechaCodeSymbolThinking;
use super::types::{SymbolEventRequest, SymbolEventResponse};
use super::ui_event::UIEvent;

#[derive(Clone)]
pub struct ToolBox {
    tools: Arc<ToolBroker>,
    symbol_broker: Arc<SymbolTrackerInline>,
    editor_parsing: Arc<EditorParsing>,
    editor_url: String,
    ui_events: UnboundedSender<UIEvent>,
}

impl ToolBox {
    pub fn new(
        tools: Arc<ToolBroker>,
        symbol_broker: Arc<SymbolTrackerInline>,
        editor_parsing: Arc<EditorParsing>,
        editor_url: String,
        ui_events: UnboundedSender<UIEvent>,
    ) -> Self {
        Self {
            tools,
            symbol_broker,
            editor_parsing,
            editor_url,
            ui_events,
        }
    }

    pub async fn should_follow_subsymbol_for_probing(
        &self,
        snippet: &Snippet,
        reason: &str,
        history: &str,
        query: &str,
        llm: LLMType,
        provider: LLMProvider,
        api_key: LLMProviderAPIKeys,
    ) -> Result<CodeSymbolShouldAskQuestionsResponse, SymbolError> {
        let file_contents = self.file_open(snippet.file_path().to_owned()).await?;
        let file_contents = file_contents.contents();
        let range = snippet.range();
        let (above, below, in_selection) = split_file_content_into_parts(&file_contents, range);
        let request = ToolInput::ProbePossibleRequest(CodeSymbolToAskQuestionsRequest::new(
            history.to_owned(),
            snippet.symbol_name().to_owned(),
            snippet.file_path().to_owned(),
            snippet.language().to_owned(),
            "".to_owned(),
            above,
            below,
            in_selection,
            llm,
            provider,
            api_key,
            // Here we can join the queries we get from the reason to the real user query
            format!(
                r"#The original user query is:
{query}

We also believe this symbol needs to be probed because of:
{reason}#"
            ),
        ));
        let _ = self.ui_events.send(UIEvent::from(request.clone()));
        self.tools
            .invoke(request)
            .await
            .map_err(|e| SymbolError::ToolError(e))?
            .get_should_probe_symbol()
            .ok_or(SymbolError::WrongToolOutput)
    }

    pub async fn probe_sub_symbols(
        &self,
        snippets: Vec<Snippet>,
        request: &SymbolToProbeRequest,
        llm: LLMType,
        provider: LLMProvider,
        api_key: LLMProviderAPIKeys,
    ) -> Result<CodeToProbeFilterResponse, SymbolError> {
        let probe_request = request.probe_request();
        let request = ToolInput::ProbeSubSymbol(CodeToEditFilterRequest::new(
            snippets,
            probe_request.to_owned(),
            llm,
            provider,
            api_key,
        ));
        let _ = self.ui_events.send(UIEvent::from(request.clone()));
        self.tools
            .invoke(request)
            .await
            .map_err(|e| SymbolError::ToolError(e))?
            .get_probe_sub_symbol()
            .ok_or(SymbolError::WrongToolOutput)
    }

    pub async fn outline_nodes_for_symbol(
        &self,
        fs_file_path: &str,
        symbol_name: &str,
    ) -> Result<String, SymbolError> {
        // send an open file request here first
        let _ = self.file_open(fs_file_path.to_owned()).await?;
        let outline_node_possible = self
            .symbol_broker
            .get_symbols_outline(fs_file_path)
            .await
            .ok_or(SymbolError::ExpectedFileToExist)?
            .into_iter()
            .find(|outline_node| outline_node.name() == symbol_name);
        if let Some(outline_node) = outline_node_possible {
            // we check for 2 things here:
            // - its either a function or a class like symbol
            // - if its a function no need to check for implementations
            // - if its a class then we still need to check for implementations
            if outline_node.is_funciton() {
                // just return this over here
                let fs_file_path = format!(
                    "{}-{}:{}",
                    outline_node.fs_file_path(),
                    outline_node.range().start_line(),
                    outline_node.range().end_line()
                );
                let content = outline_node.get_outline_short();
                Ok(format!(
                    "<outline_list>
<outline>
<symbol_name>
{symbol_name}
</symbol_name>
<file_path>
{fs_file_path}
</file_path>
<content>
{content}
</content>
</outline>
</outline_list>"
                ))
            } else {
                // we need to check for implementations as well and then return it
                let identifier_position = outline_node.identifier_range();
                // now we go to the implementations using this identifier node
                let identifier_node_positions = self
                    .go_to_implementations_exact(
                        fs_file_path,
                        &identifier_position.start_position(),
                    )
                    .await?
                    .remove_implementations_vec();
                // Now that we have the identifier positions we want to grab the
                // remaining implementations as well
                let file_paths = identifier_node_positions
                    .into_iter()
                    .map(|implementation| implementation.fs_file_path().to_owned())
                    .collect::<HashSet<String>>();
                // send a request to open all these files
                let _ = stream::iter(file_paths.clone())
                    .map(|fs_file_path| async move { self.file_open(fs_file_path).await })
                    .buffer_unordered(100)
                    .collect::<Vec<_>>()
                    .await;
                // Now all files are opened so we have also parsed them in the symbol broker
                // so we can grab the appropriate outlines properly over here
                let file_path_to_outline_nodes = stream::iter(file_paths)
                    .map(|fs_file_path| async move {
                        let symbols = self.symbol_broker.get_symbols_outline(&fs_file_path).await;
                        (fs_file_path, symbols)
                    })
                    .buffer_unordered(100)
                    .collect::<Vec<_>>()
                    .await
                    .into_iter()
                    .filter_map(
                        |(fs_file_path, outline_nodes_maybe)| match outline_nodes_maybe {
                            Some(outline_nodes) => Some((fs_file_path, outline_nodes)),
                            None => None,
                        },
                    )
                    .filter_map(|(fs_file_path, outline_nodes)| {
                        match outline_nodes
                            .into_iter()
                            .find(|outline_node| outline_node.name() == symbol_name)
                        {
                            Some(outline_node) => Some((fs_file_path, outline_node)),
                            None => None,
                        }
                    })
                    .collect::<HashMap<String, OutlineNode>>();

                // we need to get the outline for the symbol over here
                let mut outlines = vec![];
                for (fs_file_path, outline_node) in file_path_to_outline_nodes.into_iter() {
                    // Fuck it we ball, let's return the full outline here we need to truncate it later on
                    let fs_file_path = format!(
                        "{}-{}:{}",
                        outline_node.fs_file_path(),
                        outline_node.range().start_line(),
                        outline_node.range().end_line()
                    );
                    let outline = outline_node.get_outline_short();
                    outlines.push(format!(
                        r#"<outline>
<symbol_name>
{symbol_name}
</symbol_name>
<file_path>
{fs_file_path}
</file_path>
<content>
{outline}
</content>
</outline>"#
                    ))
                }

                // now add the identifier node which we are originally looking at the implementations for
                let fs_file_path = format!(
                    "{}-{}:{}",
                    outline_node.fs_file_path(),
                    outline_node.range().start_line(),
                    outline_node.range().end_line()
                );
                let outline = outline_node.get_outline_short();
                outlines.push(format!(
                    r#"<outline>
<symbol_name>
{symbol_name}
</symbol_name>
<file_path>
{fs_file_path}
</file_path>
<content>
{outline}
</content>
</outline>"#
                ));
                let joined_outlines = outlines.join("\n");
                Ok(format!(
                    r#"<outline_list>
{joined_outlines}
</outline_line>"#
                ))
            }
        } else {
            // we did not find anything here so skip this part
            Err(SymbolError::OutlineNodeNotFound(symbol_name.to_owned()))
        }
    }

    pub async fn find_symbol_to_edit(
        &self,
        symbol_to_edit: &SymbolToEdit,
    ) -> Result<OutlineNodeContent, SymbolError> {
        let outline_nodes = self
            .get_outline_nodes(symbol_to_edit.fs_file_path())
            .await
            .ok_or(SymbolError::ExpectedFileToExist)?;
        let mut filtered_outline_nodes = outline_nodes
            .into_iter()
            .filter(|outline_node| outline_node.name() == symbol_to_edit.symbol_name())
            .collect::<Vec<OutlineNodeContent>>();
        // There can be multiple nodes here which have the same name, we need to pick
        // the one we are interested in, an easy way to check this is to literally
        // check the absolute distance between the symbol we want to edit and the symbol
        filtered_outline_nodes.sort_by(|outline_node_first, outline_node_second| {
            // does it sort properly
            let distance_first: i64 = if symbol_to_edit
                .range()
                .intersects_without_byte(outline_node_first.range())
            {
                0
            } else {
                symbol_to_edit
                    .range()
                    .minimal_line_distance(outline_node_first.range())
            };

            let distance_second: i64 = if symbol_to_edit
                .range()
                .intersects_without_byte(outline_node_second.range())
            {
                0
            } else {
                symbol_to_edit
                    .range()
                    .minimal_line_distance(outline_node_second.range())
            };
            distance_first.cmp(&distance_second)
        });
        if filtered_outline_nodes.is_empty() {
            Err(SymbolError::SymbolNotFound)
        } else {
            Ok(filtered_outline_nodes.remove(0))
        }
    }

    pub fn detect_language(&self, fs_file_path: &str) -> Option<String> {
        self.editor_parsing
            .for_file_path(fs_file_path)
            .map(|ts_language_config| ts_language_config.language_str.to_owned())
    }

    pub async fn utlity_symbols_search(
        &self,
        user_query: &str,
        already_collected_definitions: &[&CodeSymbolWithThinking],
        outline_node_content: &OutlineNodeContent,
        fs_file_content: &str,
        fs_file_path: &str,
        user_context: &UserContext,
        language: &str,
        llm: LLMType,
        provider: LLMProvider,
        api_keys: LLMProviderAPIKeys,
        hub_sender: UnboundedSender<(
            SymbolEventRequest,
            tokio::sync::oneshot::Sender<SymbolEventResponse>,
        )>,
    ) -> Result<Vec<Option<(CodeSymbolWithThinking, String)>>, SymbolError> {
        // we are going to use the long context search here to check if there are
        // other utility functions we can and should use for implementing this feature
        // In our user-query we tell the LLM about what symbols are already included
        // and we ask the LLM to collect the other utility symbols which are missed

        // we have to create the query here using the outline node we are interested in
        // and the definitions which we already know about
        let request = CodeSymbolUtilityRequest::new(
            user_query.to_owned(),
            already_collected_definitions
                .into_iter()
                .map(|symbol_with_thinking| {
                    let file_path = symbol_with_thinking.file_path();
                    let symbol_name = symbol_with_thinking.code_symbol();
                    // TODO(skcd): This is horribly wrong, we want to get the full symbol
                    // over here and not just the symbol name since that does not make sense
                    // or at the very least the outline for the symbol
                    format!(
                        r#"<snippet>
<file_path>
{file_path}
</file_path>
<symbol_name>
{symbol_name}
</symbol_name>
</snippet>"#
                    )
                })
                .collect::<Vec<_>>(),
            fs_file_path.to_owned(),
            fs_file_content.to_owned(),
            outline_node_content.range().clone(),
            language.to_owned(),
            llm,
            provider,
            api_keys,
            user_context.clone(),
        );
        let tool_input = ToolInput::CodeSymbolUtilitySearch(request);
        let _ = self.ui_events.send(UIEvent::ToolEvent(tool_input.clone()));
        // These are the code symbols which are important from the global search
        // we might have some errors over here which we should fix later on, but we
        // will get on that
        // TODO(skcd): Figure out the best way to fix them
        // pick up from here, we need to run some cleanup things over here, to make sure
        // that we dont make mistakes while grabbing the code symbols
        // for now: we can assume that there are no errors over here, we can work with
        // this assumption for now
        let code_symbols = self
            .tools
            .invoke(tool_input)
            .await
            .map_err(|e| SymbolError::ToolError(e))?
            .utility_code_search_response()
            .ok_or(SymbolError::WrongToolOutput)?;

        let file_paths_to_open: Vec<String> = code_symbols
            .symbols()
            .iter()
            .map(|symbol| symbol.file_path().to_owned())
            .collect::<Vec<_>>();
        // We have the file content for the file paths which the retrival
        // engine presented us with
        let file_to_content_mapping = stream::iter(file_paths_to_open)
            .map(|file_to_open| async move {
                let tool_input = ToolInput::OpenFile(OpenFileRequest::new(
                    file_to_open.to_owned(),
                    self.editor_url.to_owned(),
                ));
                (
                    file_to_open,
                    self.tools
                        .invoke(tool_input)
                        .await
                        .map(|tool_output| tool_output.get_file_open_response()),
                )
            })
            .buffer_unordered(100)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .filter_map(
                |(fs_file_path, open_file_response)| match open_file_response {
                    Ok(Some(response)) => Some((fs_file_path, response.contents())),
                    _ => None,
                },
            )
            .collect::<HashMap<String, String>>();
        // After this we want to grab the symbol definition after looking at where
        // the symbol is in the file
        let symbols_to_grab = code_symbols.remove_symbols();
        let symbol_locations = stream::iter(symbols_to_grab)
            .map(|symbol| async {
                let symbol_name = symbol.code_symbol();
                let fs_file_path = symbol.file_path();
                if let Some(file_content) = file_to_content_mapping.get(fs_file_path) {
                    let location = self.find_symbol_in_file(symbol_name, file_content).await;
                    Some((symbol, location))
                } else {
                    None
                }
            })
            .buffer_unordered(100)
            .filter_map(|content| futures::future::ready(content))
            .collect::<Vec<_>>()
            .await;

        // We now have the locations and the symbol as well, we now ask the symbol manager
        // for the outline for this symbol
        let symbol_to_definition = stream::iter(
            symbol_locations
                .into_iter()
                .map(|symbol_location| (symbol_location, hub_sender.clone())),
        )
        .map(|((symbol, location), hub_sender)| async move {
            if let Ok(location) = location {
                // we might not get the position here for some weird reason which
                // is also fine
                let position = location.get_position();
                if let Some(position) = position {
                    let possible_file_path = self
                        .go_to_definition(fs_file_path, position)
                        .await
                        .map(|position| {
                            // there are multiple definitions here for some
                            // reason which I can't recall why, but we will
                            // always take the first one and run with it cause
                            // we then let this symbol agent take care of things
                            // TODO(skcd): The symbol needs to be on the
                            // correct file path over here
                            let symbol_file_path = position
                                .definitions()
                                .first()
                                .map(|definition| definition.file_path().to_owned());
                            symbol_file_path
                        })
                        .ok()
                        .flatten();
                    if let Some(definition_file_path) = possible_file_path {
                        let (sender, receiver) = tokio::sync::oneshot::channel();
                        // we have the possible file path over here
                        let _ = hub_sender.send((
                            SymbolEventRequest::outline(SymbolIdentifier::with_file_path(
                                symbol.code_symbol(),
                                &definition_file_path,
                            )),
                            sender,
                        ));
                        receiver
                            .await
                            .map(|response| response.to_string())
                            .ok()
                            .map(|definition_outline| (symbol, definition_outline))
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            }
        })
        .buffer_unordered(100)
        .collect::<Vec<_>>()
        .await;
        Ok(symbol_to_definition)
    }

    pub async fn check_for_followups(
        &self,
        symbol_edited: &SymbolToEdit,
        original_code: &str,
        llm: LLMType,
        provider: LLMProvider,
        api_keys: LLMProviderAPIKeys,
        hub_sender: UnboundedSender<(
            SymbolEventRequest,
            tokio::sync::oneshot::Sender<SymbolEventResponse>,
        )>,
    ) -> Result<(), SymbolError> {
        // followups here are made for checking the references or different symbols
        // or if something has changed
        // first do we show the agent the chagned data and then ask it to decide
        // where to go next or should we do something else?
        // another idea here would be to use the definitions or the references
        // of various symbols to find them and then navigate to them
        let language = self
            .editor_parsing
            .for_file_path(symbol_edited.fs_file_path())
            .map(|language_config| language_config.language_str.to_owned())
            .unwrap_or("".to_owned());
        let symbol_to_edit = self.find_symbol_to_edit(symbol_edited).await?;
        // over here we have to check if its a function or a class
        if symbol_to_edit.is_function_type() {
            // we do need to get the references over here for the function and
            // send them over as followups to check wherever they are being used
            let references = self
                .go_to_references(
                    symbol_edited.fs_file_path(),
                    &symbol_edited.range().start_position(),
                )
                .await?;
            let _ = self
                .invoke_followup_on_references(
                    symbol_edited,
                    original_code,
                    &symbol_to_edit,
                    references,
                    hub_sender,
                )
                .await;
        } else if symbol_to_edit.is_class_definition() {
            // TODO(skcd): Show the AI the changed parts over here between the original
            // code and the changed node and ask it for the symbols which we should go
            // to references for, that way we are able to do the finer garained changes
            // as and when required
            let _ = self
                .invoke_references_check_for_class_definition(
                    symbol_edited,
                    original_code,
                    &symbol_to_edit,
                    language,
                    llm,
                    provider,
                    api_keys,
                    hub_sender.clone(),
                )
                .await;
            let references = self
                .go_to_references(
                    symbol_edited.fs_file_path(),
                    &symbol_edited.range().start_position(),
                )
                .await?;
            let _ = self
                .invoke_followup_on_references(
                    symbol_edited,
                    original_code,
                    &symbol_to_edit,
                    references,
                    hub_sender,
                )
                .await;
        } else {
            // something else over here, wonder what it could be
            return Err(SymbolError::NoContainingSymbolFound);
        }
        Ok(())
    }

    async fn invoke_references_check_for_class_definition(
        &self,
        symbol_edited: &SymbolToEdit,
        original_code: &str,
        edited_symbol: &OutlineNodeContent,
        language: String,
        llm: LLMType,
        provider: LLMProvider,
        api_key: LLMProviderAPIKeys,
        hub_sender: UnboundedSender<(
            SymbolEventRequest,
            tokio::sync::oneshot::Sender<SymbolEventResponse>,
        )>,
    ) -> Result<(), SymbolError> {
        // we need to first ask the LLM for the class properties if any we have
        // to followup on if they changed
        let request = ClassSymbolFollowupRequest::new(
            symbol_edited.fs_file_path().to_owned(),
            original_code.to_owned(),
            language,
            edited_symbol.content().to_owned(),
            symbol_edited.instructions().join("\n"),
            llm,
            provider,
            api_key,
        );
        let fs_file_path = edited_symbol.fs_file_path().to_owned();
        let start_line = edited_symbol.range().start_line();
        let content_lines = edited_symbol
            .content()
            .lines()
            .enumerate()
            .into_iter()
            .map(|(index, line)| (index + start_line, line.to_owned()))
            .collect::<Vec<_>>();
        let class_memebers_to_follow = self.check_class_members_to_follow(request).await?.members();
        // now we need to get the members and schedule a followup along with the refenreces where
        // we might ber using this class
        // Now we have to get the position of the members which we want to follow-along, this is important
        // since we might have multiple members here and have to make sure that we can go-to-refernces for this
        let members_with_position = class_memebers_to_follow
            .into_iter()
            .filter_map(|member| {
                // find the position in the content where we have this member and keep track of that
                let inner_symbol = member.line();
                let found_line = content_lines
                    .iter()
                    .find(|(_, line)| line.contains(inner_symbol));
                if let Some((line_number, found_line)) = found_line {
                    let column_index = found_line.find(member.name());
                    if let Some(column_index) = column_index {
                        Some((member, Position::new(*line_number, column_index, 0)))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        stream::iter(members_with_position.into_iter().map(|(member, position)| {
            (
                member,
                position,
                fs_file_path.to_owned(),
                hub_sender.clone(),
            )
        }))
        .map(|(member, position, fs_file_path, hub_sender)| async move {
            let _ = self
                .check_followup_for_member(
                    member,
                    position,
                    &fs_file_path,
                    original_code,
                    symbol_edited,
                    edited_symbol,
                    hub_sender,
                )
                .await;
        })
        // run all these futures in parallel
        .buffer_unordered(100)
        .collect::<Vec<_>>()
        .await;
        // now we have the members and their positions along with the class defintion which we want to check anyways
        // we initial go-to-refences on all of these and try to see what we are getting

        // we also want to do a reference check for the class identifier itself, since this is also important and we
        // want to make sure that we are checking all the places where its being used
        Ok(())
    }

    async fn check_followup_for_member(
        &self,
        member: ClassSymbolMember,
        position: Position,
        // This is the file path where we want to check for the references
        fs_file_path: &str,
        original_code: &str,
        symbol_edited: &SymbolToEdit,
        edited_symbol: &OutlineNodeContent,
        hub_sender: UnboundedSender<(
            SymbolEventRequest,
            tokio::sync::oneshot::Sender<SymbolEventResponse>,
        )>,
    ) -> Result<(), SymbolError> {
        let references = self.go_to_references(fs_file_path, &position).await?;
        let reference_locations = references.locations();
        let file_paths = reference_locations
            .iter()
            .map(|reference| reference.fs_file_path().to_owned())
            .collect::<HashSet<String>>();
        // we invoke a request to open the file
        let _ = stream::iter(file_paths.clone())
            .map(|fs_file_path| async {
                // get the file content
                let _ = self.file_open(fs_file_path).await;
                ()
            })
            .buffer_unordered(100)
            .collect::<Vec<_>>()
            .await;

        // next we ask the symbol manager for all the symbols in the file and try
        // to locate our symbol inside one of the symbols?
        // once we have the outline node, we can try to understand which symbol
        // the position is part of and use that for creating the containing scope
        // of the symbol
        let mut file_path_to_outline_nodes = stream::iter(file_paths.clone())
            .map(|fs_file_path| async {
                self.get_outline_nodes_grouped(&fs_file_path)
                    .await
                    .map(|outline_nodes| (fs_file_path, outline_nodes))
            })
            .buffer_unordered(100)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .filter_map(|s| s)
            .collect::<HashMap<String, Vec<OutlineNode>>>();

        // now we have to group the files along with the positions/ranges of the references
        let mut file_paths_to_locations: HashMap<String, Vec<Range>> = Default::default();
        reference_locations.iter().for_each(|reference| {
            let file_path = reference.fs_file_path();
            let range = reference.range().clone();
            if let Some(file_pointer) = file_paths_to_locations.get_mut(file_path) {
                file_pointer.push(range);
            } else {
                file_paths_to_locations.insert(file_path.to_owned(), vec![range]);
            }
        });

        let edited_code = edited_symbol.content();
        stream::iter(
            file_paths_to_locations
                .into_iter()
                .filter_map(|(file_path, ranges)| {
                    if let Some(outline_nodes) = file_path_to_outline_nodes.remove(&file_path) {
                        Some((
                            file_path,
                            ranges,
                            hub_sender.clone(),
                            outline_nodes,
                            member.clone(),
                        ))
                    } else {
                        None
                    }
                })
                .map(
                    |(fs_file_path, ranges, hub_sender, outline_nodes, member)| {
                        ranges
                            .into_iter()
                            .map(|range| {
                                (
                                    range,
                                    hub_sender.clone(),
                                    outline_nodes.to_vec(),
                                    member.clone(),
                                )
                            })
                            .collect::<Vec<_>>()
                    },
                )
                .flatten(),
        )
        .map(|(range, hub_sender, outline_nodes, member)| async move {
            self.send_request_for_followup_class_member(
                original_code,
                edited_code,
                symbol_edited,
                member,
                range.start_position(),
                outline_nodes,
                hub_sender,
            )
            .await
        })
        .buffer_unordered(100)
        .collect::<Vec<_>>()
        .await;
        Ok(())
    }

    async fn send_request_for_followup_class_member(
        &self,
        original_code: &str,
        edited_code: &str,
        symbol_edited: &SymbolToEdit,
        member: ClassSymbolMember,
        position_to_search: Position,
        outline_nodes: Vec<OutlineNode>,
        hub_sender: UnboundedSender<(
            SymbolEventRequest,
            tokio::sync::oneshot::Sender<SymbolEventResponse>,
        )>,
    ) -> Result<(), SymbolError> {
        let outline_node_possible = outline_nodes.into_iter().find(|outline_node| {
            // we need to check if the outline node contains the range we are interested in
            outline_node.range().contains(&Range::new(
                position_to_search.clone(),
                position_to_search.clone(),
            ))
        });
        match outline_node_possible {
            Some(outline_node) => {
                // we try to find the smallest node over here which contains the position
                let child_node_possible =
                    outline_node
                        .children()
                        .into_iter()
                        .find(|outline_node_content| {
                            outline_node_content.range().contains(&Range::new(
                                position_to_search.clone(),
                                position_to_search.clone(),
                            ))
                        });

                let outline_node_fs_file_path = outline_node.content().fs_file_path();
                let outline_node_identifier_range = outline_node.content().identifier_range();
                // we can go to definition of the node and then ask the symbol for the outline over
                // here so the symbol knows about everything
                let definitions = self
                    .go_to_definition(
                        outline_node_fs_file_path,
                        outline_node_identifier_range.start_position(),
                    )
                    .await?;
                if let Some(definition) = definitions.definitions().get(0) {
                    let fs_file_path = definition.file_path();
                    let symbol_name = outline_node.name();
                    if let Some(child_node) = child_node_possible {
                        // we need to get a few lines above and below the place where the defintion is present
                        // so we can show that to the LLM properly and ask it to make changes
                        let start_line = child_node.range().start_line();
                        let content_with_line_numbers = child_node
                            .content()
                            .lines()
                            .enumerate()
                            .map(|(index, line)| (index + start_line, line.to_owned()))
                            .collect::<Vec<_>>();
                        // Now we collect 4 lines above and below the position we are interested in
                        let position_line_number = position_to_search.line() as i64;
                        let symbol_content_to_send = content_with_line_numbers
                            .into_iter()
                            .filter_map(|(line_number, line_content)| {
                                if line_number as i64 <= position_line_number + 4
                                    && line_number as i64 >= position_line_number - 4
                                {
                                    if line_number as i64 == position_line_number {
                                        // if this is the line number we are interested in then we have to highlight
                                        // this for the LLM
                                        Some(format!(
                                            r#"<line_with_reference>
{line_content}
</line_with_reference>"#
                                        ))
                                    } else {
                                        Some(line_content)
                                    }
                                } else {
                                    None
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        let instruction_prompt = self
                            .create_instruction_prompt_for_followup_class_member_change(
                                original_code,
                                edited_code,
                                &child_node,
                                &format!(
                                    "{}-{}:{}",
                                    child_node.fs_file_path(),
                                    child_node.range().start_line(),
                                    child_node.range().end_line()
                                ),
                                symbol_content_to_send,
                                member,
                                &symbol_edited,
                            );
                        // now we can send it over to the hub sender for handling the change
                        let (sender, receiver) = tokio::sync::oneshot::channel();
                        let _ = hub_sender.send((
                            SymbolEventRequest::ask_question(
                                SymbolIdentifier::with_file_path(
                                    outline_node.name(),
                                    outline_node.fs_file_path(),
                                ),
                                instruction_prompt,
                            ),
                            sender,
                        ));
                        // Figure out what to do with the receiver over here
                        let response = receiver.await;
                        // this also feels a bit iffy to me, since this will block
                        // the other requests from happening unless we do everything in parallel
                        Ok(())
                    } else {
                        // honestly this might be the case that the position where we got the reference is in some global zone
                        // which is hard to handle right now, we can just return and error and keep going
                        return Err(SymbolError::SymbolNotContainedInChild);
                    }
                    // This is now perfect since we have the symbol outline which we
                    // want to send over as context
                    // along with other metadata to create the followup-request required
                    // for making the edits as required
                } else {
                    // if there are no defintions, this is bad since we do require some kind
                    // of definition to be present here
                    return Err(SymbolError::DefinitionNotFound(
                        outline_node.name().to_owned(),
                    ));
                }
            }
            None => {
                // if there is no such outline node, then what should we do? cause we still
                // need an outline of sorts
                return Err(SymbolError::NoOutlineNodeSatisfyPosition);
            }
        }
    }

    async fn check_class_members_to_follow(
        &self,
        request: ClassSymbolFollowupRequest,
    ) -> Result<ClassSymbolFollowupResponse, SymbolError> {
        let tool_input = ToolInput::ClassSymbolFollowup(request);
        let _ = self.ui_events.send(UIEvent::ToolEvent(tool_input.clone()));
        self.tools
            .invoke(tool_input)
            .await
            .map_err(|e| SymbolError::ToolError(e))?
            .class_symbols_to_followup()
            .ok_or(SymbolError::WrongToolOutput)
    }

    async fn invoke_followup_on_references(
        &self,
        symbol_edited: &SymbolToEdit,
        original_code: &str,
        original_symbol: &OutlineNodeContent,
        references: GoToReferencesResponse,
        hub_sender: UnboundedSender<(
            SymbolEventRequest,
            tokio::sync::oneshot::Sender<SymbolEventResponse>,
        )>,
    ) -> Result<(), SymbolError> {
        let reference_locations = references.locations();
        let file_paths = reference_locations
            .iter()
            .map(|reference| reference.fs_file_path().to_owned())
            .collect::<HashSet<String>>();
        // we invoke a request to open the file
        let _ = stream::iter(file_paths.clone())
            .map(|fs_file_path| async {
                // get the file content
                let _ = self.file_open(fs_file_path).await;
                ()
            })
            .buffer_unordered(100)
            .collect::<Vec<_>>()
            .await;

        // next we ask the symbol manager for all the symbols in the file and try
        // to locate our symbol inside one of the symbols?
        // once we have the outline node, we can try to understand which symbol
        // the position is part of and use that for creating the containing scope
        // of the symbol
        let mut file_path_to_outline_nodes = stream::iter(file_paths.clone())
            .map(|fs_file_path| async {
                self.get_outline_nodes_grouped(&fs_file_path)
                    .await
                    .map(|outline_nodes| (fs_file_path, outline_nodes))
            })
            .buffer_unordered(100)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .filter_map(|s| s)
            .collect::<HashMap<String, Vec<OutlineNode>>>();

        // now we have to group the files along with the positions/ranges of the references
        let mut file_paths_to_locations: HashMap<String, Vec<Range>> = Default::default();
        reference_locations.iter().for_each(|reference| {
            let file_path = reference.fs_file_path();
            let range = reference.range().clone();
            if let Some(file_pointer) = file_paths_to_locations.get_mut(file_path) {
                file_pointer.push(range);
            } else {
                file_paths_to_locations.insert(file_path.to_owned(), vec![range]);
            }
        });

        let edited_code = original_symbol.content();
        stream::iter(
            file_paths_to_locations
                .into_iter()
                .filter_map(|(file_path, ranges)| {
                    if let Some(outline_nodes) = file_path_to_outline_nodes.remove(&file_path) {
                        Some((file_path, ranges, hub_sender.clone(), outline_nodes))
                    } else {
                        None
                    }
                })
                .map(|(fs_file_path, ranges, hub_sender, outline_nodes)| {
                    ranges
                        .into_iter()
                        .map(|range| (range, hub_sender.clone(), outline_nodes.to_vec()))
                        .collect::<Vec<_>>()
                })
                .flatten(),
        )
        .map(|(range, hub_sender, outline_nodes)| async move {
            self.send_request_for_followup(
                original_code,
                edited_code,
                symbol_edited,
                range.start_position(),
                outline_nodes,
                hub_sender,
            )
            .await
        })
        .buffer_unordered(100)
        .collect::<Vec<_>>()
        .await;
        // not entirely convinced that this is the best way to do this, but I think
        // it makes sense to do it this way
        Ok(())
    }

    fn create_instruction_prompt_for_followup_class_member_change(
        &self,
        original_code: &str,
        edited_code: &str,
        child_symbol: &OutlineNodeContent,
        file_path_for_followup: &str,
        symbol_content_with_highlight: String,
        class_memeber_change: ClassSymbolMember,
        symbol_to_edit: &SymbolToEdit,
    ) -> String {
        let member_name = class_memeber_change.name();
        let symbol_fs_file_path = symbol_to_edit.fs_file_path();
        let instructions = symbol_to_edit.instructions().join("\n");
        let child_symbol_name = child_symbol.name();
        let original_symbol_name = symbol_to_edit.symbol_name();
        let thinking = class_memeber_change.thinking();
        format!(
            r#"Another engineer has changed the member `{member_name}` in `{original_symbol_name} which is present in `{symbol_fs_file_path}
The original code for `{original_symbol_name}` is given in the <old_code> section below along with the new code which is present in <new_code> and the instructions for why the change was done in <instructions_for_change> section:
<old_code>
{original_code}
</old_code>

<new_code>
{edited_code}
</new_code>

<instructions_for_change>
{instructions}
</instructions_for_change>

The `{member_name}` is being used in `{child_symbol_name}` in the following line:
<file_path>
{file_path_for_followup}
</file_path>
<content>
{symbol_content_with_highlight}
</content>

The member for `{original_symbol_name}` which was changed is `{member_name}` and the reason we think it needs a followup change in `{child_symbol_name}` is given below:
{thinking}

Make the necessary changes if required making sure that nothing breaks"#
        )
    }

    fn create_instruction_prompt_for_followup(
        &self,
        original_code: &str,
        edited_code: &str,
        symbol_edited: &SymbolToEdit,
        child_symbol: &OutlineNodeContent,
        file_path_for_followup: &str,
        symbol_content_with_highlight: String,
    ) -> String {
        let symbol_edited_name = symbol_edited.symbol_name();
        let symbol_fs_file_path = symbol_edited.fs_file_path();
        let instructions = symbol_edited.instructions().join("\n");
        let child_symbol_name = child_symbol.name();
        format!(
            r#"Another engineer has changed the code for `{symbol_edited_name}` which is present in `{symbol_fs_file_path}`
The original code for `{symbol_edited_name}` is given below along with the new code and the instructions for why the change was done:
<old_code>
{original_code}
</old_code>

<new_code>
{edited_code}
</new_code>

<instructions_for_change>
{instructions}
</instructions_for_change>

The `{symbol_edited_name}` is being used in `{child_symbol_name}` in the following line:
<file_path>
{file_path_for_followup}
</file_path>
<content>
{symbol_content_with_highlight}
</content>

There might be need for futher changes to the `{child_symbol_name}`
Please handle these changes as required."#
        )
    }

    // we need to search for the smallest node which contains this position or range
    async fn send_request_for_followup(
        &self,
        original_code: &str,
        edited_code: &str,
        symbol_to_edit: &SymbolToEdit,
        position_to_search: Position,
        // This is pretty expensive to copy again and again
        outline_nodes: Vec<OutlineNode>,
        // this is becoming annoying now cause we will need a drain for this while
        // writing a unit-test for this
        hub_sender: UnboundedSender<(
            SymbolEventRequest,
            tokio::sync::oneshot::Sender<SymbolEventResponse>,
        )>,
    ) -> Result<(), SymbolError> {
        let outline_node_possible = outline_nodes.into_iter().find(|outline_node| {
            // we need to check if the outline node contains the range we are interested in
            outline_node.range().contains(&Range::new(
                position_to_search.clone(),
                position_to_search.clone(),
            ))
        });
        match outline_node_possible {
            Some(outline_node) => {
                // we try to find the smallest node over here which contains the position
                let child_node_possible =
                    outline_node
                        .children()
                        .into_iter()
                        .find(|outline_node_content| {
                            outline_node_content.range().contains(&Range::new(
                                position_to_search.clone(),
                                position_to_search.clone(),
                            ))
                        });

                let outline_node_fs_file_path = outline_node.content().fs_file_path();
                let outline_node_identifier_range = outline_node.content().identifier_range();
                // we can go to definition of the node and then ask the symbol for the outline over
                // here so the symbol knows about everything
                let definitions = self
                    .go_to_definition(
                        outline_node_fs_file_path,
                        outline_node_identifier_range.start_position(),
                    )
                    .await?;
                if let Some(definition) = definitions.definitions().get(0) {
                    let fs_file_path = definition.file_path();
                    let symbol_name = outline_node.name();
                    if let Some(child_node) = child_node_possible {
                        // we need to get a few lines above and below the place where the defintion is present
                        // so we can show that to the LLM properly and ask it to make changes
                        let start_line = child_node.range().start_line();
                        let content_with_line_numbers = child_node
                            .content()
                            .lines()
                            .enumerate()
                            .map(|(index, line)| (index + start_line, line.to_owned()))
                            .collect::<Vec<_>>();
                        // Now we collect 4 lines above and below the position we are interested in
                        let position_line_number = position_to_search.line() as i64;
                        let symbol_content_to_send = content_with_line_numbers
                            .into_iter()
                            .filter_map(|(line_number, line_content)| {
                                if line_number as i64 <= position_line_number + 4
                                    && line_number as i64 >= position_line_number - 4
                                {
                                    if line_number as i64 == position_line_number {
                                        // if this is the line number we are interested in then we have to highlight
                                        // this for the LLM
                                        Some(format!(
                                            r#"<line_with_reference>
{line_content}
</line_with_reference>"#
                                        ))
                                    } else {
                                        Some(line_content)
                                    }
                                } else {
                                    None
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        let instruction_prompt = self.create_instruction_prompt_for_followup(
                            original_code,
                            edited_code,
                            symbol_to_edit,
                            &child_node,
                            &format!(
                                "{}-{}:{}",
                                child_node.fs_file_path(),
                                child_node.range().start_line(),
                                child_node.range().end_line()
                            ),
                            symbol_content_to_send,
                        );
                        // now we can send it over to the hub sender for handling the change
                        let (sender, receiver) = tokio::sync::oneshot::channel();
                        let _ = hub_sender.send((
                            SymbolEventRequest::ask_question(
                                SymbolIdentifier::with_file_path(
                                    outline_node.name(),
                                    outline_node.fs_file_path(),
                                ),
                                instruction_prompt,
                            ),
                            sender,
                        ));
                        // Figure out what to do with the receiver over here
                        let response = receiver.await;
                        // this also feels a bit iffy to me, since this will block
                        // the other requests from happening unless we do everything in parallel
                        Ok(())
                    } else {
                        // honestly this might be the case that the position where we got the reference is in some global zone
                        // which is hard to handle right now, we can just return and error and keep going
                        return Err(SymbolError::SymbolNotContainedInChild);
                    }
                    // This is now perfect since we have the symbol outline which we
                    // want to send over as context
                    // along with other metadata to create the followup-request required
                    // for making the edits as required
                } else {
                    // if there are no defintions, this is bad since we do require some kind
                    // of definition to be present here
                    return Err(SymbolError::DefinitionNotFound(
                        outline_node.name().to_owned(),
                    ));
                }
            }
            None => {
                // if there is no such outline node, then what should we do? cause we still
                // need an outline of sorts
                return Err(SymbolError::NoOutlineNodeSatisfyPosition);
            }
        }
    }

    async fn go_to_references(
        &self,
        fs_file_path: &str,
        position: &Position,
    ) -> Result<GoToReferencesResponse, SymbolError> {
        let input = ToolInput::GoToReference(GoToReferencesRequest::new(
            fs_file_path.to_owned(),
            position.clone(),
            self.editor_url.to_owned(),
        ));
        let _ = self.ui_events.send(UIEvent::ToolEvent(input.clone()));
        self.tools
            .invoke(input)
            .await
            .map_err(|e| SymbolError::ToolError(e))?
            .get_references()
            .ok_or(SymbolError::WrongToolOutput)
    }

    pub async fn check_code_correctness(
        &self,
        symbol_edited: &SymbolToEdit,
        original_code: &str,
        edited_code: &str,
        // this is the context from the code edit which we want to keep using while
        // fixing
        code_edit_extra_context: &str,
        llm: LLMType,
        provider: LLMProvider,
        api_keys: LLMProviderAPIKeys,
    ) -> Result<(), SymbolError> {
        // code correction looks like this:
        // - apply the edited code to the original selection
        // - look at the code actions which are available
        // - take one of the actions or edit code as required
        // - once we have no LSP errors or anything we are good
        let instructions = symbol_edited.instructions().join("\n");
        let fs_file_path = symbol_edited.fs_file_path();
        let symbol_name = symbol_edited.symbol_name();
        let mut tries = 0;
        let max_tries = 5;
        loop {
            // keeping a try counter
            if tries >= max_tries {
                break;
            }
            tries = tries + 1;

            let mut symbol_to_edit = self.find_symbol_to_edit(symbol_edited).await?;
            let mut fs_file_content = self.file_open(fs_file_path.to_owned()).await?.contents();

            let updated_code = edited_code.to_owned();
            let edited_range = symbol_to_edit.range().clone();
            let request_id = uuid::Uuid::new_v4().to_string();
            let editor_response = self
                .apply_edits_to_editor(fs_file_path, &edited_range, &updated_code)
                .await?;

            // after applying the edits to the editor, we will need to get the file
            // contents and the symbol again
            let symbol_to_edit = self.find_symbol_to_edit(symbol_edited).await?;
            let fs_file_content = self.file_open(fs_file_path.to_owned()).await?.contents();

            // Now we check for LSP diagnostics
            let lsp_diagnostics = self
                .get_lsp_diagnostics(fs_file_path, &edited_range)
                .await?;

            // We also give it the option to edit the code as required
            if lsp_diagnostics.get_diagnostics().is_empty() {
                break;
            }

            // Now we get all the quick fixes which are available in the editor
            let quick_fix_actions = self
                .get_quick_fix_actions(fs_file_path, &edited_range, request_id.to_owned())
                .await?
                .remove_options();

            // now we can send over the request to the LLM to select the best tool
            // for editing the code out
            let selected_action = self
                .code_correctness_action_selection(
                    fs_file_path,
                    &fs_file_content,
                    &edited_range,
                    symbol_name,
                    &instructions,
                    original_code,
                    lsp_diagnostics.remove_diagnostics(),
                    quick_fix_actions.to_vec(),
                    llm.clone(),
                    provider.clone(),
                    api_keys.clone(),
                )
                .await?;

            // Now that we have the selected action, we can chose what to do about it
            // there might be a case that we have to re-write the code completely, since
            // the LLM thinks that the best thing to do, or invoke one of the quick-fix actions
            let selected_action_index = selected_action.index();

            // code edit is a special operation which is not present in the quick-fix
            // but is provided by us, the way to check this is by looking at the index and seeing
            // if its >= length of the quick_fix_actions (we append to it internally in the LLM call)
            if selected_action_index >= quick_fix_actions.len() as i64 {
                let fixed_code = self
                    .code_correctness_with_edits(
                        fs_file_path,
                        &fs_file_content,
                        symbol_to_edit.range(),
                        code_edit_extra_context.to_owned(),
                        selected_action.thinking(),
                        &instructions,
                        original_code,
                        llm.clone(),
                        provider.clone(),
                        api_keys.clone(),
                    )
                    .await?;

                // after this we have to apply the edits to the editor again and being
                // the loop again
                let _ = self
                    .apply_edits_to_editor(fs_file_path, &edited_range, &fixed_code)
                    .await?;
            } else {
                // invoke the code action over here with the ap
                let response = self
                    .invoke_quick_action(selected_action_index, &request_id)
                    .await?;
                if response.is_success() {
                    // great we have a W
                } else {
                    // boo something bad happened, we should probably log and do something about this here
                    // for now we assume its all Ws
                }
            }
        }
        Ok(())
    }

    async fn code_correctness_with_edits(
        &self,
        fs_file_path: &str,
        fs_file_content: &str,
        edited_range: &Range,
        extra_context: String,
        error_instruction: &str,
        instructions: &str,
        previous_code: &str,
        llm: LLMType,
        provider: LLMProvider,
        api_keys: LLMProviderAPIKeys,
    ) -> Result<String, SymbolError> {
        let (code_above, code_below, code_in_selection) =
            split_file_content_into_parts(fs_file_content, edited_range);
        let code_editing_error_request = ToolInput::CodeEditingError(CodeEditingErrorRequest::new(
            fs_file_path.to_owned(),
            code_above,
            code_below,
            code_in_selection,
            extra_context,
            previous_code.to_owned(),
            error_instruction.to_owned(),
            instructions.to_owned(),
            llm,
            provider,
            api_keys,
        ));
        let _ = self
            .ui_events
            .send(UIEvent::ToolEvent(code_editing_error_request.clone()));
        self.tools
            .invoke(code_editing_error_request)
            .await
            .map_err(|e| SymbolError::ToolError(e))?
            .code_editing_for_error_fix()
            .ok_or(SymbolError::WrongToolOutput)
    }

    async fn code_correctness_action_selection(
        &self,
        fs_file_path: &str,
        fs_file_content: &str,
        edited_range: &Range,
        symbol_name: &str,
        instruction: &str,
        previous_code: &str,
        diagnostics: Vec<Diagnostic>,
        quick_fix_actions: Vec<QuickFixOption>,
        llm: LLMType,
        provider: LLMProvider,
        api_keys: LLMProviderAPIKeys,
    ) -> Result<CodeCorrectnessAction, SymbolError> {
        let (code_above, code_below, code_in_selection) =
            split_file_content_into_parts(fs_file_content, edited_range);
        let request = ToolInput::CodeCorrectnessAction(CodeCorrectnessRequest::new(
            fs_file_content.to_owned(),
            fs_file_path.to_owned(),
            code_above,
            code_below,
            code_in_selection,
            symbol_name.to_owned(),
            instruction.to_owned(),
            diagnostics,
            quick_fix_actions,
            previous_code.to_owned(),
            llm,
            provider,
            api_keys,
        ));
        let _ = self.ui_events.send(UIEvent::ToolEvent(request.clone()));
        self.tools
            .invoke(request)
            .await
            .map_err(|e| SymbolError::ToolError(e))?
            .get_code_correctness_action()
            .ok_or(SymbolError::WrongToolOutput)
    }

    pub async fn code_edit(
        &self,
        fs_file_path: &str,
        file_content: &str,
        selection_range: &Range,
        extra_context: String,
        instruction: String,
        llm: LLMType,
        provider: LLMProvider,
        api_keys: LLMProviderAPIKeys,
    ) -> Result<String, SymbolError> {
        // we need to get the range above and then below and then in the selection
        let language = self
            .editor_parsing
            .for_file_path(fs_file_path)
            .map(|language_config| language_config.get_language())
            .flatten()
            .unwrap_or("".to_owned());
        let (above, below, in_range_selection) =
            split_file_content_into_parts(file_content, selection_range);
        let request = ToolInput::CodeEditing(CodeEdit::new(
            above,
            below,
            fs_file_path.to_owned(),
            in_range_selection,
            extra_context,
            language.to_owned(),
            instruction,
            llm,
            api_keys,
            provider,
        ));
        let _ = self.ui_events.send(UIEvent::ToolEvent(request.clone()));
        self.tools
            .invoke(request)
            .await
            .map_err(|e| SymbolError::ToolError(e))?
            .get_code_edit_output()
            .ok_or(SymbolError::WrongToolOutput)
    }

    async fn invoke_quick_action(
        &self,
        quick_fix_index: i64,
        request_id: &str,
    ) -> Result<LSPQuickFixInvocationResponse, SymbolError> {
        let request = ToolInput::QuickFixInvocationRequest(LSPQuickFixInvocationRequest::new(
            request_id.to_owned(),
            quick_fix_index,
            self.editor_url.to_owned(),
        ));
        self.tools
            .invoke(request)
            .await
            .map_err(|e| SymbolError::ToolError(e))?
            .get_quick_fix_invocation_result()
            .ok_or(SymbolError::WrongToolOutput)
    }

    pub async fn get_file_content(&self, fs_file_path: &str) -> Result<String, SymbolError> {
        self.symbol_broker
            .get_file_content(fs_file_path)
            .await
            .ok_or(SymbolError::UnableToReadFileContent)
    }

    pub async fn gather_important_symbols_with_definition(
        &self,
        fs_file_path: &str,
        file_content: &str,
        selection_range: &Range,
        llm: LLMType,
        provider: LLMProvider,
        api_keys: LLMProviderAPIKeys,
        query: &str,
        hub_sender: UnboundedSender<(
            SymbolEventRequest,
            tokio::sync::oneshot::Sender<SymbolEventResponse>,
        )>,
        // we get back here the defintion outline along with the reasoning on why
        // we need to look at the symbol
    ) -> Result<Vec<Option<(CodeSymbolWithThinking, String)>>, SymbolError> {
        let language = self
            .editor_parsing
            .for_file_path(fs_file_path)
            .map(|language_config| language_config.get_language())
            .flatten()
            .unwrap_or("".to_owned());
        let request = ToolInput::RequestImportantSymbols(CodeSymbolImportantRequest::new(
            None,
            vec![],
            fs_file_path.to_owned(),
            file_content.to_owned(),
            selection_range.clone(),
            llm,
            provider,
            api_keys,
            // TODO(skcd): fill in the language over here by using editor parsing
            language,
            query.to_owned(),
        ));
        let _ = self.ui_events.send(UIEvent::from(request.clone()));
        let response = self
            .tools
            .invoke(request)
            .await
            .map_err(|e| SymbolError::ToolError(e))?
            .get_important_symbols()
            .ok_or(SymbolError::WrongToolOutput)?;
        let symbols_to_grab = response
            .symbols()
            .into_iter()
            .map(|symbol| symbol.clone())
            .collect::<Vec<_>>();
        let symbol_locations = stream::iter(symbols_to_grab)
            .map(|symbol| async move {
                let symbol_name = symbol.code_symbol();
                let location = self.find_symbol_in_file(symbol_name, file_content).await;
                (symbol, location)
            })
            .buffer_unordered(100)
            .collect::<Vec<_>>()
            .await;

        // we want to grab the defintion of these symbols over here, so we can either
        // ask the hub and get it back or do something else... asking the hub is the best
        // thing to do over here
        // we now need to go to the definitions of these symbols and then ask the hub
        // manager to grab the outlines
        let symbol_to_definition = stream::iter(
            symbol_locations
                .into_iter()
                .map(|symbol_location| (symbol_location, hub_sender.clone())),
        )
        .map(|((symbol, location), hub_sender)| async move {
            if let Ok(location) = location {
                // we might not get the position here for some weird reason which
                // is also fine
                let position = location.get_position();
                if let Some(position) = position {
                    let possible_file_path = self
                        .go_to_definition(fs_file_path, position)
                        .await
                        .map(|position| {
                            // there are multiple definitions here for some
                            // reason which I can't recall why, but we will
                            // always take the first one and run with it cause
                            // we then let this symbol agent take care of things
                            // TODO(skcd): The symbol needs to be on the
                            // correct file path over here
                            let symbol_file_path = position
                                .definitions()
                                .first()
                                .map(|definition| definition.file_path().to_owned());
                            symbol_file_path
                        })
                        .ok()
                        .flatten();
                    if let Some(definition_file_path) = possible_file_path {
                        let (sender, receiver) = tokio::sync::oneshot::channel();
                        // we have the possible file path over here
                        let _ = hub_sender.send((
                            SymbolEventRequest::outline(SymbolIdentifier::with_file_path(
                                symbol.code_symbol(),
                                &definition_file_path,
                            )),
                            sender,
                        ));
                        receiver
                            .await
                            .map(|response| response.to_string())
                            .ok()
                            .map(|definition_outline| (symbol, definition_outline))
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            }
        })
        .buffer_unordered(100)
        .collect::<Vec<_>>()
        .await;
        Ok(symbol_to_definition)
    }

    async fn get_quick_fix_actions(
        &self,
        fs_file_path: &str,
        range: &Range,
        request_id: String,
    ) -> Result<GetQuickFixResponse, SymbolError> {
        let request = ToolInput::QuickFixRequest(GetQuickFixRequest::new(
            fs_file_path.to_owned(),
            self.editor_url.to_owned(),
            range.clone(),
            request_id,
        ));
        let _ = self.ui_events.send(UIEvent::ToolEvent(request.clone()));
        self.tools
            .invoke(request)
            .await
            .map_err(|e| SymbolError::ToolError(e))?
            .get_quick_fix_actions()
            .ok_or(SymbolError::WrongToolOutput)
    }

    async fn get_lsp_diagnostics(
        &self,
        fs_file_path: &str,
        range: &Range,
    ) -> Result<LSPDiagnosticsOutput, SymbolError> {
        let input = ToolInput::LSPDiagnostics(LSPDiagnosticsInput::new(
            fs_file_path.to_owned(),
            range.clone(),
            self.editor_url.to_owned(),
        ));
        let _ = self.ui_events.send(UIEvent::ToolEvent(input.clone()));
        self.tools
            .invoke(input)
            .await
            .map_err(|e| SymbolError::ToolError(e))?
            .get_lsp_diagnostics()
            .ok_or(SymbolError::WrongToolOutput)
    }

    async fn apply_edits_to_editor(
        &self,
        fs_file_path: &str,
        range: &Range,
        updated_code: &str,
    ) -> Result<EditorApplyResponse, SymbolError> {
        let input = ToolInput::EditorApplyChange(EditorApplyRequest::new(
            fs_file_path.to_owned(),
            updated_code.to_owned(),
            range.clone(),
            self.editor_url.to_owned(),
        ));
        let _ = self.ui_events.send(UIEvent::ToolEvent(input.clone()));
        self.tools
            .invoke(input)
            .await
            .map_err(|e| SymbolError::ToolError(e))?
            .get_editor_apply_response()
            .ok_or(SymbolError::WrongToolOutput)
    }

    async fn find_symbol_in_file(
        &self,
        symbol_name: &str,
        file_contents: &str,
    ) -> Result<FindInFileResponse, SymbolError> {
        // Here we are going to get the position of the symbol
        let request = ToolInput::GrepSingleFile(FindInFileRequest::new(
            file_contents.to_owned(),
            symbol_name.to_owned(),
        ));
        let _ = self.ui_events.send(UIEvent::from(request.clone()));
        self.tools
            .invoke(request)
            .await
            .map_err(|e| SymbolError::ToolError(e))?
            .grep_single_file()
            .ok_or(SymbolError::WrongToolOutput)
    }

    pub async fn filter_code_snippets_for_probing(
        &self,
        xml_string: String,
        query: String,
        llm: LLMType,
        provider: LLMProvider,
        api_keys: LLMProviderAPIKeys,
    ) -> Result<(), SymbolError> {
        Ok(())
    }

    pub async fn filter_code_snippets_in_symbol_for_editing(
        &self,
        xml_string: String,
        query: String,
        llm: LLMType,
        provider: LLMProvider,
        api_keys: LLMProviderAPIKeys,
    ) -> Result<CodeToEditSymbolResponse, SymbolError> {
        let request = ToolInput::FilterCodeSnippetsForEditingSingleSymbols(
            CodeToEditSymbolRequest::new(xml_string, query, llm, provider, api_keys),
        );
        let _ = self.ui_events.send(UIEvent::from(request.clone()));
        self.tools
            .invoke(request)
            .await
            .map_err(|e| SymbolError::ToolError(e))?
            .code_to_edit_in_symbol()
            .ok_or(SymbolError::WrongToolOutput)
    }

    /// We want to generate the outline for the symbol
    async fn get_outline_for_symbol_identifier(
        &self,
        fs_file_path: &str,
        symbol_name: &str,
        hub_sender: UnboundedSender<(
            SymbolEventRequest,
            tokio::sync::oneshot::Sender<SymbolEventResponse>,
        )>,
    ) -> Result<String, SymbolError> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let _ = hub_sender.send((
            SymbolEventRequest::outline(SymbolIdentifier::with_file_path(
                symbol_name,
                fs_file_path,
            )),
            sender,
        ));
        let response = receiver
            .await
            .map(|response| response.to_string())
            .map_err(|e| SymbolError::RecvError(e));
        // this gives us the outline we need for the outline of the symbol which
        // we are interested in
        response
    }

    pub async fn get_outline_nodes_grouped(&self, fs_file_path: &str) -> Option<Vec<OutlineNode>> {
        self.symbol_broker.get_symbols_outline(fs_file_path).await
    }

    pub async fn get_outline_nodes(&self, fs_file_path: &str) -> Option<Vec<OutlineNodeContent>> {
        self.symbol_broker
            .get_symbols_outline(&fs_file_path)
            .await
            .map(|outline_nodes| {
                // class and the functions are included here
                outline_nodes
                    .into_iter()
                    .map(|outline_node| {
                        // let children = outline_node.consume_all_outlines();
                        // outline node here contains the classes and the functions
                        // which we have to edit
                        // so one way would be to ask the LLM to edit it
                        // another is to figure out if we can show it all the functions
                        // which are present inside the class and ask it to make changes
                        let outline_content = outline_node.content().clone();
                        let all_outlines = outline_node.consume_all_outlines();
                        vec![outline_content]
                            .into_iter()
                            .chain(all_outlines)
                            .collect::<Vec<OutlineNodeContent>>()
                    })
                    .flatten()
                    .collect::<Vec<_>>()
            })
    }

    pub async fn symbol_in_range(
        &self,
        fs_file_path: &str,
        range: &Range,
    ) -> Option<Vec<OutlineNode>> {
        self.symbol_broker
            .get_symbols_in_range(fs_file_path, range)
            .await
    }

    // TODO(skcd): Use this to ask the LLM for the code snippets which need editing
    pub async fn filter_code_for_editing(
        &self,
        snippets: Vec<Snippet>,
        query: String,
        llm: LLMType,
        provider: LLMProvider,
        api_key: LLMProviderAPIKeys,
    ) -> Result<CodeToEditFilterResponse, SymbolError> {
        let request = ToolInput::FilterCodeSnippetsForEditing(CodeToEditFilterRequest::new(
            snippets, query, llm, provider, api_key,
        ));
        let _ = self.ui_events.send(UIEvent::from(request.clone()));
        self.tools
            .invoke(request)
            .await
            .map_err(|e| SymbolError::ToolError(e))?
            .code_to_edit_filter()
            .ok_or(SymbolError::WrongToolOutput)
    }

    pub async fn file_open(&self, fs_file_path: String) -> Result<OpenFileResponse, SymbolError> {
        let request = ToolInput::OpenFile(OpenFileRequest::new(
            fs_file_path,
            self.editor_url.to_owned(),
        ));
        let _ = self.ui_events.send(UIEvent::from(request.clone()));
        self.tools
            .invoke(request)
            .await
            .map_err(|e| SymbolError::ToolError(e))?
            .get_file_open_response()
            .ok_or(SymbolError::WrongToolOutput)
    }

    async fn find_in_file(
        &self,
        file_content: String,
        symbol: String,
    ) -> Result<FindInFileResponse, SymbolError> {
        let request = ToolInput::GrepSingleFile(FindInFileRequest::new(file_content, symbol));
        let _ = self.ui_events.send(UIEvent::from(request.clone()));
        self.tools
            .invoke(request)
            .await
            .map_err(|e| SymbolError::ToolError(e))?
            .grep_single_file()
            .ok_or(SymbolError::WrongToolOutput)
    }

    async fn go_to_definition(
        &self,
        fs_file_path: &str,
        position: Position,
    ) -> Result<GoToDefinitionResponse, SymbolError> {
        let request = ToolInput::GoToDefinition(GoToDefinitionRequest::new(
            fs_file_path.to_owned(),
            self.editor_url.to_owned(),
            position,
        ));
        let _ = self.ui_events.send(UIEvent::from(request.clone()));
        self.tools
            .invoke(request)
            .await
            .map_err(|e| SymbolError::ToolError(e))?
            .get_go_to_definition()
            .ok_or(SymbolError::WrongToolOutput)
    }

    // This helps us find the snippet for the symbol in the file, this is the
    // best way to do this as this is always exact and we never make mistakes
    // over here since we are using the LSP as well
    pub async fn find_snippet_for_symbol(
        &self,
        fs_file_path: &str,
        symbol_name: &str,
    ) -> Result<Snippet, SymbolError> {
        // we always open the document before asking for an outline
        let file_open_result = self.file_open(fs_file_path.to_owned()).await?;
        println!("{:?}", file_open_result);
        let language = file_open_result.language().to_owned();
        // we add the document for parsing over here
        self.symbol_broker
            .add_document(
                file_open_result.fs_file_path().to_owned(),
                file_open_result.contents(),
                language,
            )
            .await;

        // we grab the outlines over here
        let outline_nodes = self.symbol_broker.get_symbols_outline(fs_file_path).await;

        // We will either get an outline node or we will get None
        // for today, we will go with the following assumption
        // - if the document has already been open, then its good
        // - otherwise we open the document and parse it again
        if let Some(outline_nodes) = outline_nodes {
            let mut outline_nodes = self.grab_symbols_from_outline(outline_nodes, symbol_name);

            // if there are no outline nodes, then we have to skip this part
            // and keep going
            if outline_nodes.is_empty() {
                // here we need to do go-to-definition
                // first we check where the symbol is present on the file
                // and we can use goto-definition
                // so we first search the file for where the symbol is
                // this will be another invocation to the tools
                // and then we ask for the definition once we find it
                let file_data = self.file_open(fs_file_path.to_owned()).await?;
                let file_content = file_data.contents();
                // now we parse it and grab the outline nodes
                let find_in_file = self
                    .find_in_file(file_content, symbol_name.to_owned())
                    .await
                    .map(|find_in_file| find_in_file.get_position())
                    .ok()
                    .flatten();
                // now that we have a poition, we can ask for go-to-definition
                if let Some(file_position) = find_in_file {
                    let definition = self.go_to_definition(fs_file_path, file_position).await?;
                    // let definition_file_path = definition.file_path().to_owned();
                    let snippet_node = self
                        .grab_symbol_content_from_definition(symbol_name, definition)
                        .await?;
                    Ok(snippet_node)
                } else {
                    Err(SymbolError::SnippetNotFound)
                }
            } else {
                // if we have multiple outline nodes, then we need to select
                // the best one, this will require another invocation from the LLM
                // we have the symbol, we can just use the outline nodes which is
                // the first
                let outline_node = outline_nodes.remove(0);
                Ok(Snippet::new(
                    outline_node.name().to_owned(),
                    outline_node.range().clone(),
                    outline_node.fs_file_path().to_owned(),
                    outline_node.content().to_owned(),
                    outline_node,
                ))
            }
        } else {
            Err(SymbolError::OutlineNodeNotFound(symbol_name.to_owned()))
        }
    }

    // TODO(skcd): Improve this since we have code symbols which might be duplicated
    // because there can be repetitions and we can'nt be sure where they exist
    // one key hack here is that we can legit search for this symbol and get
    // to the definition of this very easily
    pub async fn important_symbols(
        &self,
        important_symbols: CodeSymbolImportantResponse,
        user_context: UserContext,
    ) -> Result<Vec<MechaCodeSymbolThinking>, SymbolError> {
        let symbols = important_symbols.symbols();
        let ordered_symbols = important_symbols.ordered_symbols();
        // there can be overlaps between these, but for now its fine
        let mut new_symbols: HashSet<String> = Default::default();
        let mut symbols_to_visit: HashSet<String> = Default::default();
        let mut final_code_snippets: HashMap<String, MechaCodeSymbolThinking> = Default::default();
        ordered_symbols.iter().for_each(|ordered_symbol| {
            let code_symbol = ordered_symbol.code_symbol().to_owned();
            if ordered_symbol.is_new() {
                new_symbols.insert(code_symbol.to_owned());
                final_code_snippets.insert(
                    code_symbol.to_owned(),
                    MechaCodeSymbolThinking::new(
                        code_symbol,
                        ordered_symbol.steps().to_owned(),
                        true,
                        ordered_symbol.file_path().to_owned(),
                        None,
                        vec![],
                        user_context.clone(),
                    ),
                );
            } else {
                symbols_to_visit.insert(code_symbol.to_owned());
                final_code_snippets.insert(
                    code_symbol.to_owned(),
                    MechaCodeSymbolThinking::new(
                        code_symbol,
                        ordered_symbol.steps().to_owned(),
                        false,
                        ordered_symbol.file_path().to_owned(),
                        None,
                        vec![],
                        user_context.clone(),
                    ),
                );
            }
        });
        for symbol in symbols.iter() {
            if !new_symbols.contains(symbol.code_symbol()) {
                symbols_to_visit.insert(symbol.code_symbol().to_owned());
                if let Some(code_snippet) = final_code_snippets.get_mut(symbol.code_symbol()) {
                    let _ = code_snippet.add_step(symbol.thinking()).await;
                }
            }
        }

        let mut mecha_symbols = vec![];

        // TODO(skcd): Refactor the code below to be the same as find_snippet_for_symbol
        // so we can contain the logic in a single place
        for (_, mut code_snippet) in final_code_snippets.into_iter() {
            // we always open the document before asking for an outline
            let file_open_result = self
                .file_open(code_snippet.fs_file_path().to_owned())
                .await?;
            println!("{:?}", file_open_result);
            let language = file_open_result.language().to_owned();
            // we add the document for parsing over here
            self.symbol_broker
                .add_document(
                    file_open_result.fs_file_path().to_owned(),
                    file_open_result.contents(),
                    language,
                )
                .await;

            // we grab the outlines over here
            let outline_nodes = self
                .symbol_broker
                .get_symbols_outline(code_snippet.fs_file_path())
                .await;

            // We will either get an outline node or we will get None
            // for today, we will go with the following assumption
            // - if the document has already been open, then its good
            // - otherwise we open the document and parse it again
            if let Some(outline_nodes) = outline_nodes {
                let mut outline_nodes =
                    self.grab_symbols_from_outline(outline_nodes, code_snippet.symbol_name());

                // if there are no outline nodes, then we have to skip this part
                // and keep going
                if outline_nodes.is_empty() {
                    // here we need to do go-to-definition
                    // first we check where the symbol is present on the file
                    // and we can use goto-definition
                    // so we first search the file for where the symbol is
                    // this will be another invocation to the tools
                    // and then we ask for the definition once we find it
                    let file_data = self
                        .file_open(code_snippet.fs_file_path().to_owned())
                        .await?;
                    let file_content = file_data.contents();
                    // now we parse it and grab the outline nodes
                    let find_in_file = self
                        .find_in_file(file_content, code_snippet.symbol_name().to_owned())
                        .await
                        .map(|find_in_file| find_in_file.get_position())
                        .ok()
                        .flatten();
                    // now that we have a poition, we can ask for go-to-definition
                    if let Some(file_position) = find_in_file {
                        let definition = self
                            .go_to_definition(&code_snippet.fs_file_path(), file_position)
                            .await?;
                        // let definition_file_path = definition.file_path().to_owned();
                        let snippet_node = self
                            .grab_symbol_content_from_definition(
                                &code_snippet.symbol_name(),
                                definition,
                            )
                            .await?;
                        code_snippet.set_snippet(snippet_node);
                    }
                } else {
                    // if we have multiple outline nodes, then we need to select
                    // the best one, this will require another invocation from the LLM
                    // we have the symbol, we can just use the outline nodes which is
                    // the first
                    let outline_node = outline_nodes.remove(0);
                    code_snippet.set_snippet(Snippet::new(
                        outline_node.name().to_owned(),
                        outline_node.range().clone(),
                        outline_node.fs_file_path().to_owned(),
                        outline_node.content().to_owned(),
                        outline_node,
                    ));
                }
            } else {
                // if this is new, then we probably do not have a file path
                // to write it to
                if !code_snippet.is_new() {
                    // its a symbol but we have nothing about it, so we log
                    // this as error for now, but later we have to figure out
                    // what to do about it
                    println!(
                        "this is pretty bad, read the comment above on what is happening {:?}",
                        &code_snippet.symbol_name()
                    );
                }
            }

            mecha_symbols.push(code_snippet);
        }
        Ok(mecha_symbols)
    }

    async fn go_to_implementations_exact(
        &self,
        fs_file_path: &str,
        position: &Position,
    ) -> Result<GoToImplementationResponse, SymbolError> {
        let _ = self.file_open(fs_file_path.to_owned()).await?;
        let request = ToolInput::SymbolImplementations(GoToImplementationRequest::new(
            fs_file_path.to_owned(),
            position.clone(),
            self.editor_url.to_owned(),
        ));
        let _ = self.ui_events.send(UIEvent::from(request.clone()));
        self.tools
            .invoke(request)
            .await
            .map_err(|e| SymbolError::ToolError(e))?
            .get_go_to_implementation()
            .ok_or(SymbolError::WrongToolOutput)
    }

    pub async fn go_to_implementation(
        &self,
        snippet: &Snippet,
        symbol_name: &str,
    ) -> Result<GoToImplementationResponse, SymbolError> {
        // LSP requies the EXACT symbol location on where to click go-to-implementation
        // since thats the case we can just open the file and then look for the
        // first occurance of the symbol and grab the location
        let file_content = self.file_open(snippet.file_path().to_owned()).await?;
        let find_in_file = self
            .find_in_file(file_content.contents(), symbol_name.to_owned())
            .await?;
        if let Some(position) = find_in_file.get_position() {
            let request = ToolInput::SymbolImplementations(GoToImplementationRequest::new(
                snippet.file_path().to_owned(),
                position,
                self.editor_url.to_owned(),
            ));
            let _ = self.ui_events.send(UIEvent::from(request.clone()));
            self.tools
                .invoke(request)
                .await
                .map_err(|e| SymbolError::ToolError(e))?
                .get_go_to_implementation()
                .ok_or(SymbolError::WrongToolOutput)
        } else {
            Err(SymbolError::ToolError(ToolError::SymbolNotFound(
                symbol_name.to_owned(),
            )))
        }
    }

    /// Grabs the symbol content and the range in the file which it is present in
    async fn grab_symbol_content_from_definition(
        &self,
        symbol_name: &str,
        definition: GoToDefinitionResponse,
    ) -> Result<Snippet, SymbolError> {
        // here we first try to open the file
        // and then read the symbols from it nad then parse
        // it out properly
        // since its very much possible that we get multiple definitions over here
        // we have to figure out how to pick the best one over here
        // TODO(skcd): This will break if we are unable to get definitions properly
        let definition = definition.definitions().remove(0);
        let _ = self.file_open(definition.file_path().to_owned()).await?;
        // grab the symbols from the file
        // but we can also try getting it from the symbol broker
        // because we are going to open a file and send a signal to the signal broker
        // let symbols = self
        //     .editor_parsing
        //     .for_file_path(definition.file_path())
        //     .ok_or(ToolError::NotSupportedLanguage)?
        //     .generate_file_outline_str(file_content.contents().as_bytes());
        let symbols = self
            .symbol_broker
            .get_symbols_outline(definition.file_path())
            .await;
        if let Some(symbols) = symbols {
            let symbols = self.grab_symbols_from_outline(symbols, symbol_name);
            // find the first symbol and grab back its content
            symbols
                .into_iter()
                .find(|symbol| symbol.name() == symbol_name)
                .map(|symbol| {
                    Snippet::new(
                        symbol.name().to_owned(),
                        symbol.range().clone(),
                        definition.file_path().to_owned(),
                        symbol.content().to_owned(),
                        symbol,
                    )
                })
                .ok_or(SymbolError::ToolError(ToolError::SymbolNotFound(
                    symbol_name.to_owned(),
                )))
        } else {
            Err(SymbolError::ToolError(ToolError::SymbolNotFound(
                symbol_name.to_owned(),
            )))
        }
    }

    fn grab_symbols_from_outline(
        &self,
        outline_nodes: Vec<OutlineNode>,
        symbol_name: &str,
    ) -> Vec<OutlineNodeContent> {
        outline_nodes
            .into_iter()
            .filter_map(|node| {
                if node.is_class() {
                    // it might either be the class itself
                    // or a function inside it so we can check for it
                    // properly here
                    if node.content().name() == symbol_name {
                        Some(vec![node.content().clone()])
                    } else {
                        Some(
                            node.children()
                                .into_iter()
                                .filter(|node| node.name() == symbol_name)
                                .map(|node| node.clone())
                                .collect::<Vec<_>>(),
                        )
                    }
                } else {
                    // we can just compare the node directly
                    // without looking at the children at this stage
                    if node.content().name() == symbol_name {
                        Some(vec![node.content().clone()])
                    } else {
                        None
                    }
                }
            })
            .flatten()
            .collect::<Vec<_>>()
    }
}
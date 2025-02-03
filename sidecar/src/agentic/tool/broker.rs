use anyhow::Context;
use async_trait::async_trait;
use llm_client::broker::LLMBroker;
use mcp_client_rs::client::Client;
use mcp_client_rs::client::ClientBuilder;
use serde::Deserialize;
use std::{collections::HashMap, sync::Arc};

use crate::{
    agentic::symbol::identifier::LLMProperties, chunking::languages::TSLanguageParsing,
    inline_completion::symbols_tracker::SymbolTrackerInline,
};

use super::mcp::integration_tool::DynamicMCPTool;
use super::{
    code_edit::{
        filter_edit::FilterEditOperationBroker, find::FindCodeSectionsToEdit,
        models::broker::CodeEditBroker, search_and_replace::SearchAndReplaceEditing,
        test_correction::TestCorrection, types::CodeEditingTool,
    },
    code_symbol::{
        apply_outline_edit_to_range::ApplyOutlineEditsToRange, correctness::CodeCorrectnessBroker,
        error_fix::CodeSymbolErrorFixBroker, find_file_for_new_symbol::FindFileForNewSymbol,
        find_symbols_to_edit_in_context::FindSymbolsToEditInContext,
        followup::ClassSymbolFollowupBroker, important::CodeSymbolImportantBroker,
        initial_request_follow::CodeSymbolFollowInitialRequestBroker,
        new_location::CodeSymbolNewLocation, new_sub_symbol::NewSubSymbolRequired,
        planning_before_code_edit::PlanningBeforeCodeEdit, probe::ProbeEnoughOrDeeper,
        probe_question_for_symbol::ProbeQuestionForSymbol,
        probe_try_hard_answer::ProbeTryHardAnswer, repo_map_search::RepoMapSearchBroker,
        reranking_symbols_for_editing_context::ReRankingSnippetsForCodeEditingContext,
        scratch_pad::ScratchPadAgentBroker, should_edit::ShouldEditCodeSymbol,
    },
    editor::apply::EditorApply,
    errors::ToolError,
    feedback::feedback::FeedbackClientGenerator,
    file::file_finder::ImportantFilesFinderBroker,
    filtering::broker::CodeToEditFormatterBroker,
    git::{diff_client::GitDiffClient, edited_files::EditedFiles},
    grep::file::FindInFile,
    input::{ToolInput, ToolInputPartial},
    lsp::{
        create_file::LSPCreateFile,
        diagnostics::LSPDiagnostics,
        file_diagnostics::FileDiagnostics,
        get_outline_nodes::OutlineNodesUsingEditorClient,
        go_to_previous_word::GoToPreviousWordClient,
        gotodefintion::LSPGoToDefinition,
        gotoimplementations::LSPGoToImplementation,
        gotoreferences::LSPGoToReferences,
        gototypedefinition::LSPGoToTypeDefinition,
        grep_symbol::GrepSymbolInCodebase,
        inlay_hints::InlayHints,
        list_files::ListFilesClient,
        open_file::LSPOpenFile,
        quick_fix::{LSPQuickFixClient, LSPQuickFixInvocationClient},
        search_file::SearchFileContentClient,
        subprocess_spawned_output::SubProcessSpawnedPendingOutputClient,
        undo_changes::UndoChangesMadeDuringExchange,
    },
    output::ToolOutput,
    plan::{
        add_steps::PlanAddStepClient, generator::StepGeneratorClient, reasoning::ReasoningClient,
        updater::PlanUpdaterClient,
    },
    r#type::{Tool, ToolRewardScale, ToolType},
    ref_filter::ref_filter::ReferenceFilterBroker,
    repo_map::generator::RepoMapGeneratorClient,
    rerank::base::ReRankBroker,
    reward::client::RewardClientGenerator,
    search::big_search::BigSearchBroker,
    session::{
        ask_followup_question::AskFollowupQuestions, attempt_completion::AttemptCompletionClient,
        chat::SessionChatClient, exchange::SessionExchangeClient,
        hot_streak::SessionHotStreakClient,
    },
    swe_bench::test_tool::SWEBenchTestTool,
    terminal::terminal::TerminalTool,
    test_runner::runner::TestRunner,
};

pub struct ToolBrokerConfiguration {
    editor_agent: Option<LLMProperties>,
    apply_edits_directly: bool,
}

impl ToolBrokerConfiguration {
    pub fn new(editor_agent: Option<LLMProperties>, apply_edits_directly: bool) -> Self {
        Self {
            editor_agent,
            apply_edits_directly,
        }
    }
}

// TODO(skcd): We want to use a different serializer and deserializer for this
// since we are going to be storing an array of tools over here, we have to make
// sure that we do not store everything about the tool but a representation of it
pub struct ToolBroker {
    tools: HashMap<ToolType, Box<dyn Tool + Send + Sync>>,
}

impl ToolBroker {
    pub fn new(
        llm_client: Arc<LLMBroker>,
        code_edit_broker: Arc<CodeEditBroker>,
        symbol_tracking: Arc<SymbolTrackerInline>,
        language_broker: Arc<TSLanguageParsing>,
        tool_broker_config: ToolBrokerConfiguration,
        // Use this if the llm we were talking to times out or does not produce
        // outout which is coherent
        // we should have finer control over the fail-over llm but for now
        // a global setting like this is fine
        fail_over_llm: LLMProperties,
    ) -> Self {
        let mut tools: HashMap<ToolType, Box<dyn Tool + Send + Sync>> = Default::default();
        tools.insert(
            ToolType::CodeEditing,
            Box::new(
                CodeEditingTool::new(
                    llm_client.clone(),
                    code_edit_broker.clone(),
                    fail_over_llm.clone(),
                )
                .set_editor_config(tool_broker_config.editor_agent.clone()),
            ),
        );
        tools.insert(ToolType::LSPDiagnostics, Box::new(LSPDiagnostics::new()));
        tools.insert(
            ToolType::FindCodeSnippets,
            Box::new(FindCodeSectionsToEdit::new(
                symbol_tracking,
                language_broker,
                code_edit_broker.clone(),
                llm_client.clone(),
            )),
        );
        tools.insert(
            ToolType::ReRank,
            Box::new(ReRankBroker::new(llm_client.clone())),
        );
        tools.insert(
            ToolType::RequestImportantSymbols,
            Box::new(CodeSymbolImportantBroker::new(
                llm_client.clone(),
                fail_over_llm.clone(),
            )),
        );
        tools.insert(
            ToolType::FindCodeSymbolsCodeBaseWide,
            Box::new(CodeSymbolImportantBroker::new(
                llm_client.clone(),
                fail_over_llm.clone(),
            )),
        );
        tools.insert(
            ToolType::UtilityCodeSymbolSearch,
            Box::new(CodeSymbolImportantBroker::new(
                llm_client.clone(),
                fail_over_llm.clone(),
            )),
        );
        tools.insert(
            ToolType::GoToDefinitions,
            Box::new(LSPGoToDefinition::new()),
        );
        tools.insert(ToolType::GoToReferences, Box::new(LSPGoToReferences::new()));
        tools.insert(ToolType::OpenFile, Box::new(LSPOpenFile::new()));
        tools.insert(ToolType::GrepInFile, Box::new(FindInFile::new()));
        tools.insert(
            ToolType::GoToImplementations,
            Box::new(LSPGoToImplementation::new()),
        );
        tools.insert(
            ToolType::FilterCodeSnippetsForEditing,
            Box::new(CodeToEditFormatterBroker::new(
                llm_client.clone(),
                fail_over_llm.clone(),
            )),
        );
        tools.insert(
            ToolType::CodeCorrectnessActionSelection,
            Box::new(CodeCorrectnessBroker::new(
                llm_client.clone(),
                fail_over_llm.clone(),
            )),
        );
        tools.insert(
            ToolType::CodeEditingForError,
            Box::new(CodeSymbolErrorFixBroker::new(
                llm_client.clone(),
                fail_over_llm.clone(),
            )),
        );
        tools.insert(
            ToolType::FilterCodeSnippetsSingleSymbolForEditing,
            Box::new(CodeToEditFormatterBroker::new(
                llm_client.clone(),
                fail_over_llm.clone(),
            )),
        );
        tools.insert(
            ToolType::EditorApplyEdits,
            Box::new(EditorApply::new(tool_broker_config.apply_edits_directly)),
        );
        tools.insert(ToolType::GetQuickFix, Box::new(LSPQuickFixClient::new()));
        tools.insert(
            ToolType::ApplyQuickFix,
            Box::new(LSPQuickFixInvocationClient::new()),
        );
        tools.insert(
            ToolType::ClassSymbolFollowup,
            Box::new(ClassSymbolFollowupBroker::new(
                llm_client.clone(),
                fail_over_llm.clone(),
            )),
        );
        tools.insert(
            ToolType::ProbePossible,
            Box::new(CodeSymbolImportantBroker::new(
                llm_client.clone(),
                fail_over_llm.clone(),
            )),
        );
        tools.insert(
            ToolType::ProbeQuestion,
            Box::new(CodeSymbolImportantBroker::new(
                llm_client.clone(),
                fail_over_llm.clone(),
            )),
        );
        tools.insert(
            ToolType::ProbeSubSymbol,
            Box::new(CodeToEditFormatterBroker::new(
                llm_client.clone(),
                fail_over_llm.clone(),
            )),
        );
        tools.insert(
            ToolType::ProbeFollowAlongSymbol,
            Box::new(CodeSymbolImportantBroker::new(
                llm_client.clone(),
                fail_over_llm.clone(),
            )),
        );
        tools.insert(
            ToolType::ProbeSummarizeAnswer,
            Box::new(CodeSymbolImportantBroker::new(
                llm_client.clone(),
                fail_over_llm.clone(),
            )),
        );
        tools.insert(
            ToolType::RepoMapSearch,
            Box::new(RepoMapSearchBroker::new(
                llm_client.clone(),
                fail_over_llm.clone(),
            )),
        );
        tools.insert(
            ToolType::ImportantFilesFinder,
            Box::new(ImportantFilesFinderBroker::new(
                llm_client.clone(),
                fail_over_llm.clone(),
            )),
        );
        // todo
        tools.insert(
            ToolType::BigSearch,
            Box::new(BigSearchBroker::new(
                llm_client.clone(),
                fail_over_llm.clone(),
            )),
        );
        tools.insert(
            ToolType::SWEBenchToolEndpoint,
            Box::new(SWEBenchTestTool::new()),
        );
        tools.insert(
            ToolType::TestCorrection,
            Box::new(TestCorrection::new(llm_client.clone())),
        );
        tools.insert(
            ToolType::CodeSymbolsToFollowInitialRequest,
            Box::new(CodeSymbolFollowInitialRequestBroker::new(
                llm_client.clone(),
            )),
        );
        tools.insert(
            ToolType::ProbeSubSymbolFiltering,
            Box::new(CodeToEditFormatterBroker::new(
                llm_client.clone(),
                fail_over_llm.clone(),
            )),
        );
        tools.insert(
            ToolType::ProbeEnoughOrDeeper,
            Box::new(ProbeEnoughOrDeeper::new(
                llm_client.clone(),
                fail_over_llm.clone(),
            )),
        );
        tools.insert(
            ToolType::ProbeCreateQuestionForSymbol,
            Box::new(ProbeQuestionForSymbol::new(
                llm_client.clone(),
                fail_over_llm.clone(),
            )),
        );
        tools.insert(
            ToolType::PlanningBeforeCodeEdit,
            Box::new(PlanningBeforeCodeEdit::new(
                llm_client.clone(),
                fail_over_llm.clone(),
            )),
        );
        tools.insert(
            ToolType::NewSubSymbolRequired,
            Box::new(NewSubSymbolRequired::new(
                llm_client.clone(),
                fail_over_llm.clone(),
            )),
        );
        tools.insert(
            ToolType::ProbeTryHardAnswer,
            Box::new(ProbeTryHardAnswer::new(
                llm_client.clone(),
                fail_over_llm.clone(),
            )),
        );
        tools.insert(
            ToolType::GrepSymbolInCodebase,
            Box::new(GrepSymbolInCodebase::new()),
        );
        tools.insert(
            ToolType::FindFileForNewSymbol,
            Box::new(FindFileForNewSymbol::new(
                llm_client.clone(),
                fail_over_llm.clone(),
            )),
        );
        tools.insert(
            ToolType::FindSymbolsToEditInContext,
            Box::new(FindSymbolsToEditInContext::new(
                llm_client.clone(),
                fail_over_llm.clone(),
            )),
        );
        tools.insert(
            ToolType::ReRankingCodeSnippetsForCodeEditingContext,
            Box::new(ReRankingSnippetsForCodeEditingContext::new(
                llm_client.clone(),
                fail_over_llm.clone(),
            )),
        );
        tools.insert(
            ToolType::ApplyOutlineEditToRange,
            Box::new(ApplyOutlineEditsToRange::new(
                llm_client.clone(),
                fail_over_llm.clone(),
                // if we are not applying directly, then we are going to stream
                // the edits to the frontend
                !tool_broker_config.apply_edits_directly,
            )),
        );
        tools.insert(
            ToolType::FilterEditOperation,
            Box::new(FilterEditOperationBroker::new(
                llm_client.clone(),
                fail_over_llm.clone(),
            )),
        );
        tools.insert(ToolType::InLayHints, Box::new(InlayHints::new()));
        tools.insert(
            ToolType::CodeSymbolNewLocation,
            Box::new(CodeSymbolNewLocation::new(
                llm_client.clone(),
                fail_over_llm.clone(),
            )),
        );
        tools.insert(
            ToolType::ShouldEditCode,
            Box::new(ShouldEditCodeSymbol::new(
                llm_client.clone(),
                fail_over_llm.clone(),
            )),
        );
        tools.insert(
            ToolType::SearchAndReplaceEditing,
            Box::new(SearchAndReplaceEditing::new(
                llm_client.clone(),
                fail_over_llm.clone(),
                Arc::new(Box::new(LSPOpenFile::new())),
            )),
        );
        tools.insert(ToolType::GitDiff, Box::new(GitDiffClient::new()));
        tools.insert(
            ToolType::OutlineNodesUsingEditor,
            Box::new(OutlineNodesUsingEditorClient::new()),
        );
        tools.insert(
            ToolType::ReferencesFilter,
            Box::new(ReferenceFilterBroker::new(
                llm_client.clone(),
                fail_over_llm.clone(),
            )),
        );
        tools.insert(
            ToolType::ScratchPadAgent,
            Box::new(ScratchPadAgentBroker::new(llm_client.clone())),
        );
        tools.insert(ToolType::EditedFiles, Box::new(EditedFiles::new()));
        tools.insert(
            ToolType::Reasoning,
            Box::new(ReasoningClient::new(llm_client.clone())),
        );
        tools.insert(
            ToolType::PlanUpdater,
            Box::new(PlanUpdaterClient::new(llm_client.clone())),
        );
        tools.insert(
            ToolType::StepGenerator,
            Box::new(StepGeneratorClient::new(llm_client.clone())),
        );
        tools.insert(ToolType::CreateFile, Box::new(LSPCreateFile::new()));
        tools.insert(
            ToolType::PlanStepAdd,
            Box::new(PlanAddStepClient::new(llm_client.clone())),
        );
        tools.insert(ToolType::FileDiagnostics, Box::new(FileDiagnostics::new()));
        tools.insert(
            ToolType::GoToPreviousWordRange,
            Box::new(GoToPreviousWordClient::new()),
        );
        tools.insert(
            ToolType::GoToTypeDefinition,
            Box::new(LSPGoToTypeDefinition::new()),
        );
        tools.insert(
            ToolType::ContextDrivenChatReply,
            Box::new(SessionChatClient::new(llm_client.clone())),
        );
        tools.insert(
            ToolType::NewExchangeDuringSession,
            Box::new(SessionExchangeClient::new()),
        );
        tools.insert(
            ToolType::UndoChangesMadeDuringSession,
            Box::new(UndoChangesMadeDuringExchange::new()),
        );
        tools.insert(
            ToolType::ContextDriveHotStreakReply,
            Box::new(SessionHotStreakClient::new(llm_client.clone())),
        );
        tools.insert(ToolType::TerminalCommand, Box::new(TerminalTool::new()));
        tools.insert(
            ToolType::SearchFileContentWithRegex,
            Box::new(SearchFileContentClient::new()),
        );
        tools.insert(ToolType::ListFiles, Box::new(ListFilesClient::new()));
        tools.insert(
            ToolType::AskFollowupQuestions,
            Box::new(AskFollowupQuestions::new()),
        );
        tools.insert(
            ToolType::AttemptCompletion,
            Box::new(AttemptCompletionClient::new()),
        );
        tools.insert(
            ToolType::RepoMapGeneration,
            Box::new(RepoMapGeneratorClient::new()),
        );
        tools.insert(
            ToolType::SubProcessSpawnedPendingOutput,
            Box::new(SubProcessSpawnedPendingOutputClient::new()),
        );
        tools.insert(ToolType::TestRunner, Box::new(TestRunner {}));
        tools.insert(
            ToolType::RewardGeneration,
            Box::new(RewardClientGenerator::new(llm_client.clone())),
        );
        tools.insert(
            ToolType::FeedbackGeneration,
            Box::new(FeedbackClientGenerator::new(llm_client)),
        );
        // we also want to add the re-ranking tool here, so we invoke it freely
        Self { tools }
    }

    /// Sets a reminder for the tool, including the name and the format of it
    pub fn get_tool_reminder(&self, tool_type: &ToolType) -> Option<String> {
        if let Some(tool) = self.tools.get(tool_type) {
            let tool_format = tool.tool_input_format();
            let tool_name = tool_type.to_string();
            Some(format!(
                r#"### {tool_name}
{tool_format}"#
            ))
        } else {
            None
        }
    }

    /// discover each MCP server in ~/.aide/config.json
    /// create dynamic tools from each server
    /// used to augument broker initialization w/MCP tools
    pub async fn with_mcp(mut self) -> anyhow::Result<Self> {
        let clients = setup_mcp_clients().await?;
        if clients.is_empty() {
            return Ok(self);
        }

        // old
        // TODO: remove before merge
        // self.tools.insert(
        //     ToolType::MCPIntegrationTool,
        //     Box::new(MCPIntegrationToolBroker::new(clients.clone())),
        // );

        // Dynamically register each server’s discovered tools as "DynamicMCPTool(tool_name)"
        let mut known_tool_names = HashMap::new(); // to ensure no duplication across servers
        for (server_name, client) in clients {
            let list_res = client.list_tools().await.context(format!(
                "Failed listing tools from server '{}'",
                server_name
            ))?;

            // e.g. "tools" is the server's Vec<{name,description,schema}>
            for tool_info in list_res.tools {
                let name = tool_info.name;
                if let Some(conflict) = known_tool_names.get(&name) {
                    anyhow::bail!(
                        "Duplicate dynamic tool name '{}' found: server '{}' vs '{}'",
                        name,
                        conflict,
                        server_name
                    );
                }
                known_tool_names.insert(name.clone(), server_name.clone());

                let dyn_tool = DynamicMCPTool::new(
                    server_name.clone(),
                    name.clone(),
                    tool_info.description,
                    tool_info.input_schema,
                    Arc::clone(&client),
                );

                self.tools
                    .insert(ToolType::DynamicMCPTool(name), Box::new(dyn_tool));
            }
        }

        Ok(self)
    }

    pub fn get_tool_description(&self, tool_type: &ToolType) -> Option<String> {
        self.tools
            .get(tool_type)
            .map(|t| format!("{}\n{}", t.tool_description(), t.tool_input_format()))
    }

    // do we need this?
    pub fn get_tool_json(&self, tool_type: &ToolType) -> Option<serde_json::Value> {
        ToolInputPartial::to_json(tool_type.clone())
    }
}

#[async_trait]
impl Tool for ToolBroker {
    async fn invoke(&self, input: ToolInput) -> Result<ToolOutput, ToolError> {
        let tool_type = input.tool_type();
        if let Some(tool) = self.tools.get(&tool_type) {
            let result = tool.invoke(input).await;
            result
        } else {
            let result = Err(ToolError::MissingTool);
            result
        }
    }

    fn tool_description(&self) -> String {
        r#"The tool broker handles all the tools which are present and provides a common api to work on top of them"#.to_owned()
    }

    fn tool_input_format(&self) -> String {
        r#"Notice that you could technically give a tool input over here, but we recommend NOT to do that and instead use individual tools if you are working with that"#.to_owned()
    }

    fn get_evaluation_criteria(&self, _trajectory_length: usize) -> Vec<String> {
        vec![]
    }

    fn get_reward_scale(&self, _trajectory_length: usize) -> Vec<ToolRewardScale> {
        vec![]
    }
}

impl ToolBroker {
    pub fn generate_evaluation_criteria(
        &self,
        tool_type: ToolType,
        trajectory_length: usize,
    ) -> Vec<String> {
        let tool_in_map = self.tools.get(&tool_type);
        match tool_in_map {
            Some(tool) => tool.get_evaluation_criteria(trajectory_length),
            None => {
                vec![]
            }
        }
    }

    pub fn generate_reward_scale(
        &self,
        tool_type: ToolType,
        trajectory_length: usize,
    ) -> Vec<ToolRewardScale> {
        // causally change the code editor tool to be the code-editing
        // tool, they both are equivalent nad yes I know how disgusting this
        // feels, trust me
        let updated_tool_type = if tool_type == ToolType::CodeEditorTool {
            ToolType::CodeEditing
        } else {
            tool_type
        };
        let tool_in_map = self.tools.get(&updated_tool_type);
        match tool_in_map {
            Some(tool) => tool.get_reward_scale(trajectory_length),
            None => {
                vec![]
            }
        }
    }
}

// Minimal code for MCP client spawner
#[derive(Deserialize)]
struct ServerConfig {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
}

#[derive(Deserialize)]
pub struct RootConfig {
    #[serde(rename = "mcpServers")]
    mcp_servers: HashMap<String, ServerConfig>,
}

/// Set up MCP clients by reading ~/.aide/config.json, spawning each server,
/// and returning a HashMap<server_name -> Arc<Client>>.
/// spawn a single MCP process per server, share references.
async fn setup_mcp_clients() -> anyhow::Result<HashMap<String, Arc<Client>>> {
    let config_path = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?
        .join(".aide/config.json");

    if !config_path.exists() {
        return Ok(HashMap::new());
    }

    let config_str = tokio::fs::read_to_string(&config_path)
        .await
        .context("Failed to read ~/.aide/config.json")?;

    let root_config: RootConfig =
        serde_json::from_str(&config_str).context("Failed to parse ~/.aide/config.json")?;

    let mut mcp_clients_map = HashMap::new();

    // For each server in the config, spawn an MCP client
    for (server_name, server_conf) in &root_config.mcp_servers {
        let mut builder = ClientBuilder::new(&server_conf.command);
        for arg in &server_conf.args {
            builder = builder.arg(arg);
        }
        for (k, v) in &server_conf.env {
            builder = builder.env(k, v);
        }

        match builder.spawn_and_initialize().await {
            Ok(client) => {
                let client_arc = Arc::new(client);
                mcp_clients_map.insert(server_name.clone(), client_arc);
                eprintln!("Initialized MCP client for '{}'", server_name);
            }
            Err(e) => {
                eprintln!(
                    "Failed to initialize MCP client for '{}': {}",
                    server_name, e
                );
                // keep trying other clients
            }
        }
    }

    Ok(mcp_clients_map)
}

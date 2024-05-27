//! Contains a lock on the different symbols and maintains them running in memory
//! this way we are able to manage different symbols and their run-time while running
//! them in a session.
//! Symbol locker has access to the whole fs file-system and can run searches
//! if the file path is not correct or incorrect, cause we have so much information
//! over here, if the symbol is properly defined we are sure to find it, even if there
//! are multiples we have enough context here to gather the information required
//! to create the correct symbol and send it over

use std::{collections::HashMap, sync::Arc};

use futures::lock::Mutex;
use tokio::sync::mpsc::UnboundedSender;

use crate::user_context::types::UserContext;

use super::{
    errors::SymbolError,
    events::types::SymbolEvent,
    identifier::{LLMProperties, MechaCodeSymbolThinking, Snippet, SymbolIdentifier},
    tool_box::ToolBox,
    types::{Symbol, SymbolEventRequest, SymbolEventResponse},
};

#[derive(Clone)]
pub struct SymbolLocker {
    symbols: Arc<
        Mutex<
            HashMap<
                // TODO(skcd): what should be the key here for this to work properly
                // cause we can have multiple symbols which share the same name
                // this probably would not happen today but would be good to figure
                // out at some point
                SymbolIdentifier,
                // this is the channel which we use to talk to this particular symbol
                // and everything related to it
                UnboundedSender<(
                    SymbolEvent,
                    tokio::sync::oneshot::Sender<SymbolEventResponse>,
                )>,
            >,
        >,
    >,
    // this is the main communication channel which we can use to send requests
    // to the right symbol
    hub_sender: UnboundedSender<(
        SymbolEventRequest,
        tokio::sync::oneshot::Sender<SymbolEventResponse>,
    )>,
    tools: Arc<ToolBox>,
    llm_properties: LLMProperties,
    user_context: UserContext,
}

impl SymbolLocker {
    pub fn new(
        hub_sender: UnboundedSender<(
            SymbolEventRequest,
            tokio::sync::oneshot::Sender<SymbolEventResponse>,
        )>,
        tools: Arc<ToolBox>,
        llm_properties: LLMProperties,
        user_context: UserContext,
    ) -> Self {
        Self {
            symbols: Arc::new(Mutex::new(HashMap::new())),
            hub_sender,
            tools,
            llm_properties,
            user_context,
        }
    }

    pub async fn process_request(
        &self,
        request_event: (
            SymbolEventRequest,
            tokio::sync::oneshot::Sender<SymbolEventResponse>,
        ),
    ) {
        let request = request_event.0;
        let sender = request_event.1;
        let symbol_identifier = request.symbol().clone();
        let mut does_exist = false;
        {
            if self.symbols.lock().await.get(&symbol_identifier).is_none() {
                // if symbol already exists then we can just forward it to the symbol
            } else {
                does_exist = false;
                // the symbol does not exist and we have to create it first and then send it over
            }
        }

        if !does_exist {
            if let Some(fs_file_path) = symbol_identifier.fs_file_path() {
                // grab the snippet for this symbol
                let snippet = self
                    .tools
                    .find_snippet_for_symbol(&fs_file_path, symbol_identifier.symbol_name())
                    .await;
                if let Ok(snippet) = snippet {
                    // the symbol does not exist so we have to make sure that we can send it over somehow
                    let mecha_code_symbol_thinking = MechaCodeSymbolThinking::new(
                        symbol_identifier.symbol_name().to_owned(),
                        vec![],
                        false,
                        symbol_identifier.fs_file_path().expect("to present"),
                        Some(snippet),
                        vec![],
                        self.user_context.clone(),
                    );
                    // we create the symbol over here, but what about the context, I want
                    // to pass it to the symbol over here
                    let _ = self.create_symbol_agent(mecha_code_symbol_thinking).await;
                } else {
                    // we are fucked over here since we didn't find a snippet for the symbol
                    // which is supposed to have some presence in the file
                    todo!("no snippet found for the snippet, we are screwed over here, look at the comment above");
                }
            } else {
                // well this kind of sucks, cause we do not know where the symbol is anymore
                // worst case this means that we have to create a new symbol somehow
                // best case this could mean that we fucked up majorly somewhere... what should we do???
                todo!("we are mostly fucked if this is the case, we have to figure out how to handle the request coming in but not having the file path later on")
            }
        }

        // at this point we have also tried creating the symbol agent, so we can start logging it
        {
            if let Some(symbol) = self.symbols.lock().await.get(&symbol_identifier) {
                let _ = symbol.send((request.remove_event(), sender));
            }
        }
    }

    pub async fn create_symbol_agent(
        &self,
        request: MechaCodeSymbolThinking,
    ) -> Result<(), SymbolError> {
        // say we create the symbol agent, what happens next
        // the agent can have its own events which it might need to do, including the
        // followups or anything else
        // the user might have some events to send
        // other agents might also want to talk to it for some information
        let symbol_identifier = request.to_symbol_identifier();
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel::<(
            SymbolEvent,
            tokio::sync::oneshot::Sender<SymbolEventResponse>,
        )>();
        {
            let mut symbols = self.symbols.lock().await;
            symbols.insert(symbol_identifier.clone(), sender);
        }

        // now we create the symbol and let it rip
        let symbol = Symbol::new(
            symbol_identifier,
            request,
            self.hub_sender.clone(),
            self.tools.clone(),
            self.llm_properties.clone(),
        )
        .await?;

        // now we let it rip, we give the symbol the receiver and ask it
        // to go crazy with it
        tokio::spawn(async move {
            let _ = symbol.run(receiver).await;
        });
        // fin
        Ok(())
    }
}
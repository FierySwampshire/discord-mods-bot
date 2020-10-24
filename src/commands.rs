use crate::{
    api,
    state_machine::{CharacterSet, StateMachine},
};
use reqwest::blocking::Client as HttpClient;
use serenity::{model::channel::Message, prelude::Context};
use std::{collections::HashMap, sync::Arc};

const PREFIX: &'static str = "?";
pub(crate) type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
pub(crate) type GuardFn = fn(&Args) -> Result<bool>;

struct Command {
    guard: GuardFn,
    ptr: Box<dyn for<'m> Fn(Args<'m>) -> Result<()> + Send + Sync>,
}

impl Command {
    fn authorize(&self, args: &Args) -> Result<bool> {
        (self.guard)(&args)
    }

    fn call(&self, args: Args) -> Result<()> {
        (self.ptr)(args)
    }
}

pub struct Args<'m> {
    pub http: &'m HttpClient,
    pub cx: &'m Context,
    pub msg: &'m Message,
    pub params: HashMap<&'m str, &'m str>,
}

pub(crate) struct Commands {
    state_machine: StateMachine<Arc<Command>>,
    client: HttpClient,
    menu: Option<HashMap<&'static str, (&'static str, GuardFn)>>,
}

impl Commands {
    pub(crate) fn new() -> Self {
        Self {
            state_machine: StateMachine::new(),
            client: HttpClient::new(),
            menu: Some(HashMap::new()),
        }
    }

    pub(crate) fn add(
        &mut self,
        command: &'static str,
        handler: impl Fn(Args) -> Result<()> + Send + Sync + 'static,
    ) {
        self.add_protected(command, handler, |_| Ok(true));
    }

    pub(crate) fn add_protected(
        &mut self,
        command: &'static str,
        handler: impl Fn(Args) -> Result<()> + Send + Sync + 'static,
        guard: GuardFn,
    ) {
        info!("Adding command {}", &command);
        let mut state = 0;

        let mut opt_lambda_state = None;
        let mut opt_final_states = vec![];

        command
            .split(' ')
            .filter(|segment| segment.len() > 0)
            .enumerate()
            .for_each(|(i, segment)| {
                if let Some(name) = key_value_pair(segment) {
                    if let Some(lambda) = opt_lambda_state {
                        state = add_key_value(&mut self.state_machine, name, lambda);
                        self.state_machine.add_next_state(state, lambda);
                        opt_final_states.push(state);
                    } else {
                        opt_final_states.push(state);
                        state = add_space(&mut self.state_machine, state, i);
                        opt_lambda_state = Some(state);
                        state = add_key_value(&mut self.state_machine, name, state);
                        self.state_machine
                            .add_next_state(state, opt_lambda_state.unwrap());
                        opt_final_states.push(state);
                    }
                } else {
                    opt_lambda_state = None;
                    opt_final_states.truncate(0);
                    state = add_space(&mut self.state_machine, state, i);

                    if segment.starts_with("```\n") && segment.ends_with("```") {
                        let name = &segment[4..segment.len() - 3];
                        state = add_code_segment_multi_line(&mut self.state_machine, name, state);
                    } else if segment.starts_with("```") && segment.ends_with("```") {
                        let name = &segment[3..segment.len() - 3];
                        state =
                            add_code_segment_single_line(&mut self.state_machine, name, state, 3);
                    } else if segment.starts_with("`") && segment.ends_with("`") {
                        let name = &segment[1..segment.len() - 1];
                        state =
                            add_code_segment_single_line(&mut self.state_machine, name, state, 1);
                    } else if segment.starts_with("{") && segment.ends_with("}") {
                        let name = &segment[1..segment.len() - 1];
                        state = add_dynamic_segment(&mut self.state_machine, name, state);
                    } else if segment.ends_with("...") {
                        let name = &segment[..segment.len() - 3];
                        state = add_remaining_segment(&mut self.state_machine, name, state);
                    } else {
                        segment.chars().for_each(|ch| {
                            state = self.state_machine.add(state, CharacterSet::from_char(ch))
                        });
                    }
                }
            });

        let handler = Arc::new(Command {
            guard,
            ptr: Box::new(handler),
        });

        if opt_lambda_state.is_some() {
            opt_final_states.iter().for_each(|state| {
                self.state_machine.set_final_state(*state);
                self.state_machine.set_handler(*state, handler.clone());
            });
        } else {
            self.state_machine.set_final_state(state);
            self.state_machine.set_handler(state, handler.clone());
        }
    }

    pub(crate) fn help(
        &mut self,
        cmd: &'static str,
        desc: &'static str,
        handler: impl Fn(Args) -> Result<()> + Send + Sync + 'static,
    ) {
        self.help_protected(cmd, desc, handler, |_| Ok(true));
    }

    pub(crate) fn help_protected(
        &mut self,
        cmd: &'static str,
        desc: &'static str,
        handler: impl Fn(Args) -> Result<()> + Send + Sync + 'static,
        guard: GuardFn,
    ) {
        let base_cmd = &cmd[1..];
        info!("Adding command ?help {}", &base_cmd);
        let mut state = 0;

        self.menu.as_mut().map(|menu| {
            menu.insert(cmd, (desc, guard));
            menu
        });

        state = add_help_menu(&mut self.state_machine, base_cmd, state);
        self.state_machine.set_final_state(state);
        self.state_machine.set_handler(
            state,
            Arc::new(Command {
                guard,
                ptr: Box::new(handler),
            }),
        );
    }

    pub(crate) fn menu(&mut self) -> Option<HashMap<&'static str, (&'static str, GuardFn)>> {
        self.menu.take()
    }

    pub(crate) fn execute<'m>(&'m self, cx: Context, msg: Message) {
        let message = &msg.content;
        if !msg.is_own(&cx) && message.starts_with(PREFIX) {
            self.state_machine.process(message).map(|matched| {
                info!("Processing command: {}", message);
                let args = Args {
                    http: &self.client,
                    cx: &cx,
                    msg: &msg,
                    params: matched.params,
                };
                info!("Checking permissions");
                match matched.handler.authorize(&args) {
                    Ok(true) => {
                        info!("Executing command");
                        if let Err(e) = matched.handler.call(args) {
                            error!("{}", e);
                        }
                    }
                    Ok(false) => {
                        info!("Not executing command, unauthorized");
                        if let Err(e) =
                            api::send_reply(&args, "You do not have permission to run this command")
                        {
                            error!("{}", e);
                        }
                    }
                    Err(e) => error!("{}", e),
                }
            });
        }
    }
}

fn key_value_pair(s: &'static str) -> Option<&'static str> {
    s.match_indices("={}")
        .nth(0)
        .map(|pair| {
            let name = &s[0..pair.0];
            if name.len() > 0 {
                Some(name)
            } else {
                None
            }
        })
        .flatten()
}

fn add_space<T>(state_machine: &mut StateMachine<T>, mut state: usize, i: usize) -> usize {
    if i > 0 {
        let mut char_set = CharacterSet::from_char(' ');
        char_set.insert('\n');

        state = state_machine.add(state, char_set);
        state_machine.add_next_state(state, state);
    }
    state
}

fn add_help_menu<T>(
    mut state_machine: &mut StateMachine<T>,
    cmd: &'static str,
    mut state: usize,
) -> usize {
    "?help".chars().for_each(|ch| {
        state = state_machine.add(state, CharacterSet::from_char(ch));
    });
    state = add_space(&mut state_machine, state, 1);
    cmd.chars().for_each(|ch| {
        state = state_machine.add(state, CharacterSet::from_char(ch));
    });

    state
}

fn add_dynamic_segment<T>(
    state_machine: &mut StateMachine<T>,
    name: &'static str,
    mut state: usize,
) -> usize {
    let mut char_set = CharacterSet::any();
    char_set.remove(' ');
    state = state_machine.add(state, char_set);
    state_machine.add_next_state(state, state);
    state_machine.start_parse(state, name);
    state_machine.end_parse(state);

    state
}

fn add_remaining_segment<T>(
    state_machine: &mut StateMachine<T>,
    name: &'static str,
    mut state: usize,
) -> usize {
    let char_set = CharacterSet::any();
    state = state_machine.add(state, char_set);
    state_machine.add_next_state(state, state);
    state_machine.start_parse(state, name);
    state_machine.end_parse(state);

    state
}

fn add_code_segment_multi_line<T>(
    state_machine: &mut StateMachine<T>,
    name: &'static str,
    mut state: usize,
) -> usize {
    state = state_machine.add(state, CharacterSet::from_char('`'));
    state = state_machine.add(state, CharacterSet::from_char('`'));
    state = state_machine.add(state, CharacterSet::from_char('`'));

    let lambda = state;

    let mut char_set = CharacterSet::any();
    char_set.remove('`');
    char_set.remove(' ');
    char_set.remove('\n');
    state = state_machine.add(state, char_set);
    state_machine.add_next_state(state, state);

    state = state_machine.add(state, CharacterSet::from_char('\n'));

    state_machine.add_next_state(lambda, state);

    state = state_machine.add(state, CharacterSet::any());
    state_machine.add_next_state(state, state);
    state_machine.start_parse(state, name);
    state_machine.end_parse(state);

    state = state_machine.add(state, CharacterSet::from_char('`'));
    state = state_machine.add(state, CharacterSet::from_char('`'));
    state = state_machine.add(state, CharacterSet::from_char('`'));

    state
}

fn add_code_segment_single_line<T>(
    state_machine: &mut StateMachine<T>,
    name: &'static str,
    mut state: usize,
    n_backticks: usize,
) -> usize {
    (0..n_backticks).for_each(|_| {
        state = state_machine.add(state, CharacterSet::from_char('`'));
    });
    state = state_machine.add(state, CharacterSet::any());
    state_machine.add_next_state(state, state);
    state_machine.start_parse(state, name);
    state_machine.end_parse(state);
    (0..n_backticks).for_each(|_| {
        state = state_machine.add(state, CharacterSet::from_char('`'));
    });

    state
}

fn add_key_value<T>(
    state_machine: &mut StateMachine<T>,
    name: &'static str,
    mut state: usize,
) -> usize {
    name.chars().for_each(|c| {
        state = state_machine.add(state, CharacterSet::from_char(c));
    });
    state = state_machine.add(state, CharacterSet::from_char('='));

    let mut char_set = CharacterSet::any();
    char_set.remove(' ');
    char_set.remove('\n');
    state = state_machine.add(state, char_set);
    state_machine.add_next_state(state, state);
    state_machine.start_parse(state, name);
    state_machine.end_parse(state);

    state
}

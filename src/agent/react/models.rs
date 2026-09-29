pub const DEFAULT_MAX_TURNS: usize = 12;

#[derive(Debug, Clone)]
pub enum Step {
    Thought {
        turn: usize,
        content: String,
    },
    Answer {
        turn: usize,
        content: String,
    },
    Action {
        turn: usize,
        name: String,
        arguments: String,
    },
    Observation {
        turn: usize,
        name: String,
        output: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Termination {
    ModelFinished,
    MaxTurns,
    EmptyReply,
}

#[derive(Debug)]
pub struct Outcome {
    pub answer: String,
    pub turns: usize,
    pub termination: Termination,
}

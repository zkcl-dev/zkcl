use icu_properties::props::{
    ExtendedPictographic, GraphemeClusterBreak as Break, IndicConjunctBreak as Indic,
};
use icu_properties::{CodePointMapData, CodePointSetData};

#[derive(Default, Clone, Copy)]
enum IndicTail {
    #[default]
    None,
    Consonant,
    Linker,
}

#[derive(Default, Clone, Copy)]
enum EmojiTail {
    #[default]
    None,
    Pictograph,
    Zwj,
}

#[derive(Default, Clone, Copy)]
pub struct Graphemes {
    previous: Option<Break>,
    regional_odd: bool,
    indic: IndicTail,
    emoji: EmojiTail,
}

impl Graphemes {
    pub fn push(&mut self, character: char) -> bool {
        let current = CodePointMapData::<Break>::new().get(character);
        let indic = CodePointMapData::<Indic>::new().get(character);
        let pictograph = CodePointSetData::new::<ExtendedPictographic>().contains(character);
        let boundary = self.boundary(current, indic, pictograph);
        self.previous = Some(current);
        self.regional_odd = current == Break::RegionalIndicator && !self.regional_odd;
        self.indic = match (self.indic, indic) {
            (_, Indic::Consonant) => IndicTail::Consonant,
            (IndicTail::Consonant | IndicTail::Linker, Indic::Linker) => IndicTail::Linker,
            (tail, Indic::Extend) => tail,
            _ => IndicTail::None,
        };
        self.emoji = match (self.emoji, current, pictograph) {
            (_, _, true) | (EmojiTail::Pictograph, Break::Extend, false) => EmojiTail::Pictograph,
            (EmojiTail::Pictograph, Break::ZWJ, false) => EmojiTail::Zwj,
            _ => EmojiTail::None,
        };
        boundary
    }

    fn boundary(&self, current: Break, indic: Indic, pictograph: bool) -> bool {
        let Some(previous) = self.previous else {
            return true;
        };
        match (previous, current) {
            (Break::CR, Break::LF) => false,
            (Break::CR | Break::LF | Break::Control, _)
            | (_, Break::CR | Break::LF | Break::Control) => true,
            (Break::L, Break::L | Break::V | Break::LV | Break::LVT)
            | (Break::LV | Break::V, Break::V | Break::T)
            | (Break::LVT | Break::T, Break::T)
            | (_, Break::Extend | Break::ZWJ | Break::SpacingMark)
            | (Break::Prepend, _) => false,
            _ if indic == Indic::Consonant && matches!(self.indic, IndicTail::Linker) => false,
            _ if pictograph && matches!(self.emoji, EmojiTail::Zwj) => false,
            (Break::RegionalIndicator, Break::RegionalIndicator) => !self.regional_odd,
            _ => true,
        }
    }
}

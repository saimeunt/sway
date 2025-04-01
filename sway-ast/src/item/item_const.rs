use crate::priv_prelude::*;

#[derive(Clone, Debug, Serialize)]
pub struct ItemConst {
    pub visibility: Option<PubToken>,
    pub const_token: ConstToken,
    pub name: Ident,
    pub ty: Ty,
    pub eq_token_opt: Option<EqToken>,
    pub expr_opt: Option<Expr>,
    pub semicolon_token: SemicolonToken,
}

impl Spanned for ItemConst {
    fn span(&self) -> Span {
        let start = match &self.visibility {
            Some(pub_token) => pub_token.span(),
            None => self.const_token.span(),
        };
        let end = match &self.expr_opt {
            Some(expr) => expr.span(),
            None => self.ty.span(),
        };
        Span::join(start, &end)
    }
}

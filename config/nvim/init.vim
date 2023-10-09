autocmd BufRead,BufNewFile *.txt,*.md setlocal spell spelllang=en_us

set nocompatible
filetype plugin on
syntax on


call plug#begin()

Plug 'hrsh7th/cmp-nvim-lsp'
Plug 'hrsh7th/nvim-cmp'
Plug 'hrsh7th/vim-vsnip'

Plug 'nvim-lua/plenary.nvim'
Plug 'scalameta/nvim-metals'

Plug 'junegunn/fzf'
Plug 'junegunn/fzf.vim'

call plug#end()



autocmd BufRead,BufNewFile *.txt,*.md setlocal spell spelllang=en_us

set nocompatible
filetype plugin on
syntax on
set background=light


call plug#begin()

" color...
Plug 'folke/tokyonight.nvim'

Plug 'hrsh7th/cmp-nvim-lsp'
Plug 'hrsh7th/nvim-cmp'
Plug 'hrsh7th/vim-vsnip'

Plug 'nvim-lua/plenary.nvim'
Plug 'scalameta/nvim-metals'

" Fuzzy finding
Plug 'junegunn/fzf'
Plug 'junegunn/fzf.vim'

" Surround text objects with quotes, etc.
Plug 'tpope/vim-surround'

" Repeat surrounds, etc.
Plug 'tpope/vim-repeat'

Plug 'lukas-reineke/indent-blankline.nvim'

call plug#end()


colorscheme tokyonight-night



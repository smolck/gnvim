use log::{debug, error};

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gdk;
use glib;
use gtk;
use neovim_lib::neovim::{Neovim, UiOption};
use neovim_lib::neovim_api::NeovimApi;
use neovim_lib::NeovimApiAsync;
use neovim_lib::Value;

use gtk::prelude::*;

use crate::nvim_bridge::{
    CmdlinePos, CmdlineSpecialChar, DefaultColorsSet, GnvimEvent,
    GridCursorGoto, GridResize, HlAttrDefine, Message, ModeChange, ModeInfo,
    ModeInfoSet, Notify, OptionSet, RedrawEvent, Request, TablineUpdate,
};
use crate::ui::cmdline::Cmdline;
use crate::ui::color::{Color, Highlight};
#[cfg(feature = "libwebkit2gtk")]
use crate::ui::cursor_tooltip::{CursorTooltip, Gravity};
use crate::ui::font::Font;
use crate::ui::grid::Grid;
use crate::ui::popupmenu::Popupmenu;
use crate::ui::tabline::Tabline;

type Grids = HashMap<u64, Grid>;

#[derive(Default)]
pub struct HlDefs {
    hl_defs: HashMap<u64, Highlight>,

    pub default_fg: Color,
    pub default_bg: Color,
    pub default_sp: Color,
}

impl HlDefs {
    pub fn get_mut(&mut self, id: &u64) -> Option<&mut Highlight> {
        self.hl_defs.get_mut(id)
    }

    pub fn get(&self, id: &u64) -> Option<&Highlight> {
        self.hl_defs.get(id)
    }

    pub fn insert(&mut self, id: u64, hl: Highlight) -> Option<Highlight> {
        self.hl_defs.insert(id, hl)
    }
}

struct ResizeOptions {
    font: Font,
    line_space: i64,
}

/// Internal structure for `UI` to work on.
struct UIState {
    /// All grids currently in the UI.
    grids: Grids,
    /// Highlight definitions.
    hl_defs: HlDefs,
    /// Mode infos. When a mode is activated, the activated mode is passed
    /// to the gird(s).
    mode_infos: Vec<ModeInfo>,
    /// Id of the current active grid.
    current_grid: u64,

    popupmenu: Popupmenu,
    cmdline: Cmdline,
    tabline: Tabline,
    #[cfg(feature = "libwebkit2gtk")]
    cursor_tooltip: CursorTooltip,

    /// Overlay contains our grid(s) and popupmenu.
    #[allow(unused)]
    overlay: gtk::Overlay,

    /// Source id for delayed call to ui_try_resize.
    resize_source_id: Rc<RefCell<Option<glib::SourceId>>>,
    /// Resize options that is some if a resize should be send to nvim on flush.
    resize_on_flush: Option<ResizeOptions>,
}

/// Main UI structure.
pub struct UI {
    /// Main window.
    win: gtk::ApplicationWindow,
    /// Neovim instance.
    nvim: Rc<RefCell<Neovim>>,
    /// Channel to receive event from nvim.
    rx: glib::Receiver<Message>,
    /// Our internal state, containing basically everything we manipulate
    /// when we receive an event from nvim.
    state: Rc<RefCell<UIState>>,
}

impl UI {
    /// Creates new UI.
    ///
    /// * `app` - GTK application for the UI.
    /// * `rx` - Channel to receive nvim UI events.
    /// * `nvim` - Neovim instance to use. Should be the same that is the source
    ///            of `rx` events.
    pub fn init(
        app: &gtk::Application,
        rx: glib::Receiver<Message>,
        window_size: (i32, i32),
        nvim: Rc<RefCell<Neovim>>,
    ) -> Self {
        // Create the main window.
        let window = gtk::ApplicationWindow::new(app);
        window.set_title("Neovim");
        window.set_default_size(window_size.0, window_size.1);

        // Top level widget.
        let b = gtk::Box::new(gtk::Orientation::Vertical, 0);
        window.add(&b);

        let tabline = Tabline::new(nvim.clone());
        b.pack_start(&tabline.get_widget(), false, false, 0);

        // Our root widget.
        let overlay = gtk::Overlay::new();
        b.pack_start(&overlay, true, true, 0);

        let box_ = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        overlay.add(&box_);

        // Create hl defs and initialize 0th element because we'll need to have
        // something that is accessible for the default grid that we're gonna
        // make next.
        let mut hl_defs = HlDefs::default();
        hl_defs.insert(0, Highlight::default());

        // Create default grid.
        let mut grid = Grid::new(1);
        box_.pack_start(&grid.widget(), true, true, 0);

        // When resizing our window (main grid), we'll have to tell neovim to
        // resize it self also. The notify to nvim is send with a small delay,
        // so we don't spam it multiple times a second. source_id is used to
        // track the function timeout. This timeout might be canceled in
        // redraw even handler if we receive a message that changes the size
        // of the main grid.
        let source_id = Rc::new(RefCell::new(None));
        grid.connect_da_resize(clone!(nvim, source_id => move |rows, cols| {

            // Set timeout to notify nvim about the new size.
            let new = gtk::timeout_add(30, clone!(nvim, source_id => move || {
                let mut nvim = nvim.borrow_mut();
                nvim.ui_try_resize_async(cols as i64, rows as i64)
                    .cb(|res| {
                        if let Err(err) = res {
                            error!("Error: failed to resize nvim when grid size changed ({:?})", err);
                        }
                    })
                .call();

                // Set the source_id to none, so we don't accidentally remove
                // it since it used at this point.
                source_id.borrow_mut().take();

                Continue(false)
            }));

            let mut source_id = source_id.borrow_mut();
            // If we have earlier timeout, remove it.
            if let Some(old) = source_id.take() {
                glib::source::source_remove(old);
            }

            *source_id = Some(new);

            false
        }));

        // Mouse button press event.
        grid.connect_mouse_button_press_events(
            clone!(nvim => move |button, row, col| {
                let mut nvim = nvim.borrow_mut();
                let input = format!("<{}Mouse><{},{}>", button, col, row);
                nvim.input(&input).expect("Couldn't send mouse input");

                Inhibit(false)
            }),
        );

        // Mouse button release events.
        grid.connect_mouse_button_release_events(
            clone!(nvim => move |button, row, col| {
                let mut nvim = nvim.borrow_mut();
                let input = format!("<{}Release><{},{}>", button, col, row);
                nvim.input(&input).expect("Couldn't send mouse input");

                Inhibit(false)
            }),
        );

        // Mouse drag events.
        grid.connect_motion_events_for_drag(
            clone!(nvim => move |button, row, col| {
                let mut nvim = nvim.borrow_mut();
                let input = format!("<{}Drag><{},{}>", button, col, row);
                nvim.input(&input).expect("Couldn't send mouse input");

                Inhibit(false)
            }),
        );

        // Scrolling events.
        grid.connect_scroll_events(clone!(nvim => move |dir, row, col| {
            let mut nvim = nvim.borrow_mut();
            let input = format!("<{}><{},{}>", dir, col, row);
            nvim.input(&input).expect("Couldn't send mouse input");

            Inhibit(false)
        }));

        // IMMulticontext is used to handle most of the inputs.
        let im_context = gtk::IMMulticontext::new();
        im_context.set_use_preedit(false);
        im_context.connect_commit(clone!(nvim => move |_, input| {
            // "<" needs to be escaped for nvim.input()
            let nvim_input = input.replace("<", "<lt>");

            let mut nvim = nvim.borrow_mut();
            nvim.input(&nvim_input).expect("Couldn't send input");
        }));

        window.connect_key_press_event(clone!(nvim, im_context => move |_, e| {
            if im_context.filter_keypress(e) {
                Inhibit(true)
            } else {
                if let Some(input) = event_to_nvim_input(e) {
                    let mut nvim = nvim.borrow_mut();
                    nvim.input(input.as_str()).expect("Couldn't send input");
                    return Inhibit(true);
                } else {
                    debug!(
                        "Failed to turn input event into nvim key (keyval: {})",
                        e.get_keyval()
                    )
                }

                Inhibit(false)
            }
        }));

        window.connect_key_release_event(clone!(im_context => move |_, e| {
            im_context.filter_keypress(e);
            Inhibit(false)
        }));

        window.connect_focus_in_event(clone!(im_context => move |_, _| {
            im_context.focus_in();
            Inhibit(false)
        }));

        window.connect_focus_out_event(clone!(im_context => move |_, _| {
            im_context.focus_out();
            Inhibit(false)
        }));

        let cmdline = Cmdline::new(&overlay, nvim.clone());
        #[cfg(feature = "libwebkit2gtk")]
        let cursor_tooltip = CursorTooltip::new(&overlay);

        window.show_all();

        grid.set_im_context(&im_context);

        cmdline.hide();
        #[cfg(feature = "libwebkit2gtk")]
        cursor_tooltip.hide();

        let mut grids = HashMap::new();
        grids.insert(1, grid);

        UI {
            win: window,
            rx,
            state: Rc::new(RefCell::new(UIState {
                grids,
                mode_infos: vec![],
                current_grid: 1,
                popupmenu: Popupmenu::new(&overlay, nvim.clone()),
                cmdline,
                overlay,
                tabline,
                #[cfg(feature = "libwebkit2gtk")]
                cursor_tooltip,
                resize_source_id: source_id,
                hl_defs,
                resize_on_flush: None,
            })),
            nvim,
        }
    }

    /// Starts to listen events from `rx` (e.g. from nvim) and processing those.
    /// Think this as the "main" function of the UI.
    pub fn start(self) {
        let UI {
            rx,
            state,
            win,
            nvim,
        } = self;

        gtk::timeout_add(
            33,
            clone!(state => move || {
                let state = state.borrow();
                // Tick the current active grid.
                let grid =
                    state.grids.get(&state.current_grid).unwrap();
                grid.tick();

                glib::Continue(true)
            }),
        );

        rx.attach(None, move |message| {
            match message {
                // Handle a notify.
                Message::Notify(notify) => {
                    let mut state = state.borrow_mut();

                    handle_notify(&win, &notify, &mut state, nvim.clone());
                }
                // Handle a request.
                Message::Request(tx, request) => {
                    let mut state = state.borrow_mut();
                    let res = handle_request(&request, &mut state);
                    tx.send(res).expect("Failed to respond to a request");
                }
                // Handle close.
                Message::Close => {
                    win.close();
                    return Continue(false);
                }
            }

            Continue(true)
        });
    }
}

fn handle_request(
    request: &Request,
    state: &mut UIState,
) -> Result<Value, Value> {
    match request {
        #[cfg(feature = "libwebkit2gtk")]
        Request::CursorTooltipStyles => {
            let styles = state.cursor_tooltip.get_styles();

            let res: Vec<Value> =
                styles.into_iter().map(|s| s.into()).collect();

            Ok(res.into())
        }
        #[cfg(not(feature = "libwebkit2gtk"))]
        Request::CursorTooltipStyles => {
            Err("Cursor tooltip is not supported in this build".into())
        }
    }
}

fn handle_notify(
    window: &gtk::ApplicationWindow,
    notify: &Notify,
    state: &mut UIState,
    nvim: Rc<RefCell<Neovim>>,
) {
    match notify {
        Notify::RedrawEvent(events) => {
            handle_redraw_event(window, events, state, nvim);
        }
        Notify::GnvimEvent(event) => match event {
            Ok(event) => handle_gnvim_event(event, state, nvim),
            Err(err) => {
                let mut nvim = nvim.borrow_mut();
                nvim.command_async(&format!(
                    "echom \"Failed to parse gnvim notify: '{}'\"",
                    err
                ))
                .cb(|res| match res {
                    Ok(_) => {}
                    Err(err) => {
                        error!("Failed to execute nvim command: {}", err)
                    }
                })
                .call();
            }
        },
    }
}

fn handle_gnvim_event(
    event: &GnvimEvent,
    state: &mut UIState,
    nvim: Rc<RefCell<Neovim>>,
) {
    match event {
        GnvimEvent::EnableExtTabline(option) => {
            let mut nvim = nvim.borrow_mut();
            nvim.set_option(UiOption::ExtTabline(*option))
                .unwrap_or_else(|err| {
                    nvim.command_async(&format!(
                        "echo \"Failed to enable ext_tabline: '{}'\"",
                        err
                    ));
                });

            if !*option {
                state.tabline.get_widget().hide();
                return;
            }

            match (nvim.get_current_tabpage(), nvim.list_tabpages()) {
                (Ok(current), Ok(tabpages)) => {
                    let tabpages = tabpages
                        .iter()
                        .map(|x| (x.clone(), String::from("")))
                        .collect();

                    state.tabline.update(&mut nvim, current, tabpages);
                }
                (Err(err), Ok(_)) => {
                    nvim.command_async(&format!(
                            "echo \"Unable to get current tabpage for updating tabline: '{}'\"", err));
                }
                (Ok(_), Err(err)) => {
                    nvim.command_async(&format!(
                            "echo \"Unable to get tabpages for updating tabline: '{}'\"", err));
                }
                (Err(get_current_err), Err(list_tabpages_err)) => {
                    nvim.command_async(&format!(
                            "echo \"Unable to get current tabpage for updating tabline: '{}'\n
                            Unable to get tabpages for updating tabline: '{}'\"", get_current_err, list_tabpages_err));
                }
            }
        }
        GnvimEvent::EnableExtPmenu(option) => {
            let mut nvim = nvim.borrow_mut();
            nvim.set_option(UiOption::ExtPopupmenu(*option))
                .unwrap_or_else(|err| {
                    nvim.command_async(&format!(
                        "echo \"Failed to enable ext_popupmenu: '{}'\"",
                        err
                    ));
                });
        }
        GnvimEvent::EnableExtCmdline(option) => {
            let mut nvim = nvim.borrow_mut();
            nvim.set_option(UiOption::ExtCmdline(*option))
                .unwrap_or_else(|err| {
                    nvim.command_async(&format!(
                        "echo \"Failed to enable ext_cmdline: '{}'\"",
                        err
                    ));
                });
        }

        GnvimEvent::SetGuiColors(colors) => {
            state.popupmenu.set_colors(colors.pmenu, &state.hl_defs);
            state.tabline.set_colors(colors.tabline, &state.hl_defs);
            state.cmdline.set_colors(colors.cmdline, &state.hl_defs);
            state
                .cmdline
                .wildmenu_set_colors(&colors.wildmenu, &state.hl_defs);
        }
        GnvimEvent::CompletionMenuToggleInfo => {
            state.popupmenu.toggle_show_info()
        }
        GnvimEvent::PopupmenuWidth(width) => {
            state.popupmenu.set_width(*width as i32);
        }
        GnvimEvent::PopupmenuWidthDetails(width) => {
            state.popupmenu.set_width_details(*width as i32);
        }
        GnvimEvent::PopupmenuShowMenuOnAllItems(should_show) => {
            state.popupmenu.set_show_menu_on_all_items(*should_show);
        }
        GnvimEvent::Unknown(msg) => {
            debug!("Received unknown GnvimEvent: {}", msg);
        }

        #[cfg(not(feature = "libwebkit2gtk"))]
        GnvimEvent::CursorTooltipLoadStyle(..)
        | GnvimEvent::CursorTooltipShow(..)
        | GnvimEvent::CursorTooltipHide
        | GnvimEvent::CursorTooltipSetStyle(..) => {
            let mut nvim = nvim.borrow_mut();
            nvim.command_async(
                "echom \"Cursor tooltip not supported in this build\"",
            )
            .cb(|res| match res {
                Ok(_) => {}
                Err(err) => error!("Failed to execute nvim command: {}", err),
            })
            .call();
        }

        #[cfg(feature = "libwebkit2gtk")]
        GnvimEvent::CursorTooltipLoadStyle(..)
        | GnvimEvent::CursorTooltipShow(..)
        | GnvimEvent::CursorTooltipHide
        | GnvimEvent::CursorTooltipSetStyle(..) => match event {
            GnvimEvent::CursorTooltipLoadStyle(path) => {
                if let Err(err) = state.cursor_tooltip.load_style(path.clone())
                {
                    let mut nvim = nvim.borrow_mut();
                    nvim.command_async(&format!(
                        "echom \"Cursor tooltip load style failed: '{}'\"",
                        err
                    ))
                    .cb(|res| match res {
                        Ok(_) => {}
                        Err(err) => {
                            error!("Failed to execute nvim command: {}", err)
                        }
                    })
                    .call();
                }
            }
            GnvimEvent::CursorTooltipShow(content, row, col) => {
                state.cursor_tooltip.show(content.clone());

                let grid = state.grids.get(&state.current_grid).unwrap();
                let rect = grid.get_rect_for_cell(*row, *col);

                state.cursor_tooltip.move_to(&rect);
            }
            GnvimEvent::CursorTooltipHide => state.cursor_tooltip.hide(),
            GnvimEvent::CursorTooltipSetStyle(style) => {
                state.cursor_tooltip.set_style(style)
            }
            _ => unreachable!(),
        },
    }
}

fn handle_redraw_event(
    window: &gtk::ApplicationWindow,
    events: &Vec<RedrawEvent>,
    state: &mut UIState,
    nvim: Rc<RefCell<Neovim>>,
) {
    for event in events {
        match event {
            RedrawEvent::SetTitle(evt) => {
                evt.iter().for_each(|title| {
                    window.set_title(title);
                });
            }
            RedrawEvent::GridLine(evt) => {
                evt.iter().for_each(|line| {
                    let grid = state.grids.get(&line.grid).unwrap();
                    grid.put_line(line, &state.hl_defs);
                });
            }
            RedrawEvent::GridCursorGoto(evt) => {
                evt.iter().for_each(
                    |GridCursorGoto {
                         grid: grid_id,
                         row,
                         col,
                     }| {
                        // Gird cursor goto sets the current cursor to grid_id,
                        // so we'll need to handle that here...
                        let grid = if *grid_id != state.current_grid {
                            // ...so if the grid_id is not same as the state tells us,
                            // set the previous current grid to inactive state.
                            state
                                .grids
                                .get(&state.current_grid)
                                .unwrap()
                                .set_active(false);
                            state.current_grid = *grid_id;

                            // And set the new current grid to active.
                            let grid = state.grids.get(grid_id).unwrap();
                            grid.set_active(true);
                            grid
                        } else {
                            state.grids.get(grid_id).unwrap()
                        };

                        // And after all that, set the current grid's cursor position.
                        grid.cursor_goto(*row, *col);
                    },
                );
            }
            RedrawEvent::GridResize(evt) => {
                evt.iter().for_each(
                    |GridResize {
                         grid,
                         width,
                         height,
                     }| {
                        let grid = state.grids.get(grid).unwrap();
                        grid.resize(*width, *height);
                    },
                );
            }
            RedrawEvent::GridClear(evt) => {
                evt.iter().for_each(|grid| {
                    let grid = state.grids.get(grid).unwrap();
                    grid.clear(&state.hl_defs);
                });
            }
            RedrawEvent::GridScroll(evt) => {
                evt.iter().for_each(|info| {
                    let grid = state.grids.get(&info.grid).unwrap();
                    grid.scroll(info.reg, info.rows, info.cols, &state.hl_defs);

                    let mut nvim = nvim.borrow_mut();
                    // Since nvim doesn't have its own 'scroll' autocmd, we'll
                    // have to do it on our own. This use useful for the cursor tooltip.
                    nvim.command_async("if exists('#User#GnvimScroll') | doautocmd User GnvimScroll | endif")
                     .cb(|res| match res {
                         Ok(_) => {}
                         Err(err) => error!("GnvimScroll error: {:?}", err),
                     })
                     .call();
                });
            }
            RedrawEvent::DefaultColorsSet(evt) => {
                evt.iter().for_each(|DefaultColorsSet { fg, bg, sp }| {
                    state.hl_defs.default_fg = *fg;
                    state.hl_defs.default_bg = *bg;
                    state.hl_defs.default_sp = *sp;

                    {
                        // NOTE(ville): Not sure if these are actually needed.
                        let hl = state.hl_defs.get_mut(&0).unwrap();
                        hl.foreground = Some(*fg);
                        hl.background = Some(*bg);
                        hl.special = Some(*sp);
                    }

                    for grid in state.grids.values() {
                        grid.redraw(&state.hl_defs);
                    }

                    #[cfg(feature = "libwebkit2gtk")]
                    state.cursor_tooltip.set_colors(*fg, *bg);
                });
            }
            RedrawEvent::HlAttrDefine(evt) => {
                evt.iter().for_each(|HlAttrDefine { id, hl }| {
                    state.hl_defs.insert(*id, *hl);
                });
            }
            RedrawEvent::OptionSet(evt) => {
                evt.iter().for_each(|opt| match opt {
                    OptionSet::GuiFont(font) => {
                        let font =
                            Font::from_guifont(font).unwrap_or(Font::default());

                        let mut opts =
                            state.resize_on_flush.take().unwrap_or_else(|| {
                                let grid = state.grids.get(&1).unwrap();
                                ResizeOptions {
                                    font: grid.get_font(),
                                    line_space: grid.get_line_space(),
                                }
                            });

                        opts.font = font;

                        state.resize_on_flush = Some(opts);
                    }
                    OptionSet::LineSpace(val) => {
                        let mut opts =
                            state.resize_on_flush.take().unwrap_or_else(|| {
                                let grid = state.grids.get(&1).unwrap();
                                ResizeOptions {
                                    font: grid.get_font(),
                                    line_space: grid.get_line_space(),
                                }
                            });

                        opts.line_space = *val;

                        state.resize_on_flush = Some(opts);
                    }
                    OptionSet::NotSupported(name) => {
                        debug!("Not supported option set: {}", name);
                    }
                });
            }
            RedrawEvent::ModeInfoSet(evt) => {
                evt.iter().for_each(|ModeInfoSet { mode_info, .. }| {
                    state.mode_infos = mode_info.clone();
                });
            }
            RedrawEvent::ModeChange(evt) => {
                evt.iter().for_each(|ModeChange { index, .. }| {
                    let mode = state.mode_infos.get(*index as usize).unwrap();
                    // Broadcast the mode change to all grids.
                    // TODO(ville): It might be enough to just set the mode to the
                    //              current active grid.
                    for grid in state.grids.values() {
                        grid.set_mode(mode);
                    }
                });
            }
            RedrawEvent::SetBusy(busy) => {
                for grid in state.grids.values() {
                    grid.set_busy(*busy);
                }
            }
            RedrawEvent::Flush() => {
                for grid in state.grids.values() {
                    grid.flush(&state.hl_defs);
                }

                if let Some(opts) = state.resize_on_flush.take() {
                    for grid in state.grids.values() {
                        grid.update_cell_metrics(
                            opts.font.clone(),
                            opts.line_space,
                        );
                    }

                    let grid = state.grids.get(&1).unwrap();
                    let (cols, rows) = grid.calc_size();

                    // Cancel any possible delayed call for ui_try_resize.
                    let mut id = state.resize_source_id.borrow_mut();
                    if let Some(id) = id.take() {
                        glib::source::source_remove(id);
                    }

                    nvim.borrow_mut().ui_try_resize_async(cols as i64, rows as i64)
                        .cb(|res| {
                            if let Err(err) = res {
                                error!("Error: failed to resize nvim on line space change ({:?})", err);
                            }
                        })
                    .call();

                    state.popupmenu.set_font(opts.font.clone(), &state.hl_defs);
                    state.cmdline.set_font(opts.font.clone(), &state.hl_defs);
                    state.tabline.set_font(opts.font.clone(), &state.hl_defs);
                    #[cfg(feature = "libwebkit2gtk")]
                    state.cursor_tooltip.set_font(opts.font.clone());

                    state.cmdline.set_line_space(opts.line_space);
                    state
                        .popupmenu
                        .set_line_space(opts.line_space, &state.hl_defs);
                    state
                        .tabline
                        .set_line_space(opts.line_space, &state.hl_defs);
                }
            }
            RedrawEvent::PopupmenuShow(evt) => {
                evt.iter().for_each(|popupmenu| {
                    state
                        .popupmenu
                        .set_items(popupmenu.items.clone(), &state.hl_defs);

                    let grid = state.grids.get(&state.current_grid).unwrap();
                    let rect =
                        grid.get_rect_for_cell(popupmenu.row, popupmenu.col);

                    state.popupmenu.set_anchor(rect);
                    state
                        .popupmenu
                        .select(popupmenu.selected as i32, &state.hl_defs);

                    state.popupmenu.show();

                    // If the cursor tooltip is visible at the same time, move
                    // it out of our way.
                    #[cfg(feature = "libwebkit2gtk")]
                    {
                        if state.cursor_tooltip.is_visible() {
                            if state.popupmenu.is_above_anchor() {
                                state
                                    .cursor_tooltip
                                    .force_gravity(Some(Gravity::Down));
                            } else {
                                state
                                    .cursor_tooltip
                                    .force_gravity(Some(Gravity::Up));
                            }

                            state.cursor_tooltip.refresh_position();
                        }
                    }
                });
            }
            RedrawEvent::PopupmenuHide() => {
                state.popupmenu.hide();

                // Undo any force positioning of cursor tool tip that might
                // have occured on popupmenu show.
                #[cfg(feature = "libwebkit2gtk")]
                {
                    state.cursor_tooltip.force_gravity(None);
                    state.cursor_tooltip.refresh_position();
                }
            }
            RedrawEvent::PopupmenuSelect(evt) => {
                evt.iter().for_each(|selected| {
                    state.popupmenu.select(*selected as i32, &state.hl_defs);
                });
            }
            RedrawEvent::TablineUpdate(evt) => {
                evt.iter().for_each(|TablineUpdate { current, tabs }| {
                    let mut nvim = nvim.borrow_mut();
                    state.tabline.update(
                        &mut nvim,
                        current.clone(),
                        tabs.clone(),
                    );
                });
            }
            RedrawEvent::CmdlineShow(evt) => {
                evt.iter().for_each(|cmdline_show| {
                    state.cmdline.show(cmdline_show, &state.hl_defs);
                });
            }
            RedrawEvent::CmdlineHide() => {
                state.cmdline.hide();
            }
            RedrawEvent::CmdlinePos(evt) => {
                evt.iter().for_each(|CmdlinePos { pos, level }| {
                    state.cmdline.set_pos(*pos, *level);
                });
            }
            RedrawEvent::CmdlineSpecialChar(evt) => {
                evt.iter().for_each(
                    |CmdlineSpecialChar {
                         character: ch,
                         shift,
                         level,
                     }| {
                        state.cmdline.show_special_char(
                            ch.clone(),
                            *shift,
                            *level,
                        );
                    },
                );
            }
            RedrawEvent::CmdlineBlockShow(evt) => {
                evt.iter().for_each(|show| {
                    state.cmdline.show_block(show, &state.hl_defs);
                });
            }
            RedrawEvent::CmdlineBlockAppend(evt) => {
                evt.iter().for_each(|line| {
                    state.cmdline.block_append(line, &state.hl_defs);
                });
            }
            RedrawEvent::CmdlineBlockHide() => {
                state.cmdline.hide_block();
            }
            RedrawEvent::WildmenuShow(evt) => {
                evt.iter().for_each(|items| {
                    state.cmdline.wildmenu_show(&items.0);
                });
            }
            RedrawEvent::WildmenuHide() => {
                state.cmdline.wildmenu_hide();
            }
            RedrawEvent::WildmenuSelect(evt) => {
                evt.iter().for_each(|item| {
                    state.cmdline.wildmenu_select(*item);
                });
            }
            RedrawEvent::Ignored(_) => (),
            RedrawEvent::Unknown(e) => {
                debug!("Received unknown redraw event: {}", e);
            }
        }
    }
}

fn keyname_to_nvim_key(s: &str) -> Option<&str> {
    // Sourced from python-gui.
    match s {
        "slash" => Some("/"),
        "backslash" => Some("\\"),
        "dead_circumflex" => Some("^"),
        "at" => Some("@"),
        "numbersign" => Some("#"),
        "dollar" => Some("$"),
        "percent" => Some("%"),
        "ampersand" => Some("&"),
        "asterisk" => Some("*"),
        "parenleft" => Some("("),
        "parenright" => Some(")"),
        "underscore" => Some("_"),
        "plus" => Some("+"),
        "minus" => Some("-"),
        "bracketleft" => Some("["),
        "bracketright" => Some("]"),
        "braceleft" => Some("{"),
        "braceright" => Some("}"),
        "dead_diaeresis" => Some("\""),
        "dead_acute" => Some("\'"),
        "less" => Some("<"),
        "greater" => Some(">"),
        "comma" => Some(","),
        "period" => Some("."),
        "BackSpace" => Some("BS"),
        "Insert" => Some("Insert"),
        "Return" => Some("CR"),
        "Escape" => Some("Esc"),
        "Delete" => Some("Del"),
        "Page_Up" => Some("PageUp"),
        "Page_Down" => Some("PageDown"),
        "Enter" => Some("CR"),
        "ISO_Left_Tab" => Some("Tab"),
        "Tab" => Some("Tab"),
        "Up" => Some("Up"),
        "Down" => Some("Down"),
        "Left" => Some("Left"),
        "Right" => Some("Right"),
        "Home" => Some("Home"),
        "End" => Some("End"),
        "F1" => Some("F1"),
        "F2" => Some("F2"),
        "F3" => Some("F3"),
        "F4" => Some("F4"),
        "F5" => Some("F5"),
        "F6" => Some("F6"),
        "F7" => Some("F7"),
        "F8" => Some("F8"),
        "F9" => Some("F9"),
        "F10" => Some("F10"),
        "F11" => Some("F11"),
        "F12" => Some("F12"),
        _ => None,
    }
}

fn event_to_nvim_input(e: &gdk::EventKey) -> Option<String> {
    let mut input = String::from("");

    let keyval = e.get_keyval();
    let keyname = gdk::keyval_name(keyval)?;

    let state = e.get_state();

    if state.contains(gdk::ModifierType::SHIFT_MASK) {
        input.push_str("S-");
    }
    if state.contains(gdk::ModifierType::CONTROL_MASK) {
        input.push_str("C-");
    }
    if state.contains(gdk::ModifierType::MOD1_MASK) {
        input.push_str("A-");
    }

    if keyname.chars().count() > 1 {
        let n = keyname_to_nvim_key(keyname.as_str())?;
        input.push_str(n);
    } else {
        input.push(gdk::keyval_to_unicode(keyval)?);
    }

    Some(format!("<{}>", input))
}
